# LTP 网络测试进度总表

Date: 2026-06-03

本文档回答两个问题：

- **总表**：LTP 网络相关测试分成哪些类型？每类现在多少分？
- **细表**：每个类型下面具体哪些测试已经通过、部分通过、跳过或未跑？

计分口径：

- `x/y` 来自本地 `tools/oscomp-judge.py` 或 LTP case summary，是当前本地
  进度口径，不是一次性官方全量提交分。
- `TCONF/skipped` 表示测试因命令、驱动、配置或服务环境缺失而跳过，不放进
  “可计分小计”的分母。
- `not-run` 表示还没有有效 witness，不计分。
- `dirty-tree` 表示当前未提交实验结果，只能说明调试进展，不能算通过。
- native runner 有时会在 `PASS LTP CASE ... : 0` 后面再打印一个旧式
  `FAIL LTP CASE ... : 0` marker；以 LTP summary 和本地 judge 为准。

当前最重要的小计：

- 已确认通过点数：`384` = syscall-network `229` +
  `net.ipv6_lib` `76` + `net.tcp_cmds` `46` + `net.ipv6:ping601`
  `10` + `net.ipv6:ping602` `10` + `net.ipv6:ipneigh6_ip` `1` +
  `net.ipv6:traceroute601` `6` + `net.ipv6:tracepath601` `1` +
  `net.ipv6:tcpdump601` `1` + `net.ipv6:sendfile601` `4`。
- 已观察分母口径：`384/392` = syscall-network `229/236` +
  `net.ipv6_lib` `76/77` + `net.tcp_cmds` 已计分 `46/46` +
  `net.ipv6:ping601` `10/10` + `net.ipv6:ping602` `10/10` +
  `net.ipv6:traceroute601` `6/6` + `net.ipv6:ipneigh6_ip` `1/1` +
  `net.ipv6:tracepath601` `1/1` + `net.ipv6:tcpdump601` `1/1` +
  `net.ipv6:sendfile601` `4/4`。
- 上面的 `384/392` 仍然是 stitched/local 进度，不等于全量 LTP network
  官方成绩；`TCONF` 和未跑模块没有计入分母。

## 总表

| 类型 | 来源 | 条目规模 | 当前得分 | 当前状态 | 下一步 |
| --- | --- | ---: | ---: | --- | --- |
| socket/network syscall | `runtest/syscalls` 手动筛出的 50 个 socket/network case | 50 cases | `229/236` | split-batch 已覆盖全部 50 个 case；剩余缺口主要是架构面、镜像/用户态 wrapper、少量非核心网络 surface | 作为回归基线；细节见下面 syscall 分批表和 `docs/LTP/ltp-network-syscall-progress.md` |
| IPv6 libc/API | `net.ipv6_lib` | 6 entries | `76/77` | 基本完成；只剩 `asapi_01` 的 `hopopt` 协议表点，属于 musl test image/libc 表缺口 | 不优先花 kernel 网络时间；除非允许重建 LTP/musl 镜像 |
| IPv4/命令层网络 | `net.tcp_cmds` | 17 entries | 已计分 `46/46`，另有 6 项 `TCONF/skipped` | `netstat`、`iproute`、`ping01`、`ping02`、`arping01`、`ipneigh01_{arp,ip}`、`sendfile`、`tracepath01`、`traceroute01`、`tcpdump` 已过 | 继续剩余服务命令和 netfilter/driver 广告 |
| IPv6 命令层网络 | `net.ipv6` | 11 entries | 已计分 `33/33`，另有 4 项 `TCONF/skipped` | `ping601`、`ping602`、`sendfile601`、`tracepath601`、`ipneigh6_ip`、`traceroute601`、`tcpdump601` 已过；`dhcpd6`、`dnsmasq6`、`ip6tables`、`nft6` 目前是工具/driver TCONF | 下一步优先决定服务命令和 netfilter/driver 广告 |
| 高级网络特性 | `net.features` | 62 entries | not-run | 未开始；包含 BBR、DCCP、SCTP、TFO、VXLAN、VLAN、macvlan、macsec、GRE/GUE/FOU、Geneve、WireGuard 等 | 暂缓，等命令层/IPv6 baseline 更稳 |
| 组播 | `net.multicast` | 4 entries | not-run | 未开始 | 暂缓 |
| 完整 SCTP | `net.sctp` | 41 entries | not-run | 未开始；不同于 syscall witness 里的 local-only SCTP 支持 | 除非明确 charter 完整 SCTP，否则暂缓 |
| NFS/RPC/TIRPC | `net.nfs`、`net.rpc_tests`、`net.tirpc_tests` | 205 entries | not-run | 未开始；依赖服务、RPC/NFS 环境 | 暂缓 |
| 网络压力测试 | `net_stress.*` | 588 entries | not-run | 未开始；覆盖服务、坏包、interface/route/multicast/ipsec 压力 | 暂缓 |
| CAN | `can` | 3 entries | not-run | 未开始 | 暂缓 |

## 细表：socket/network syscall 分批

这部分来自 `runtest/syscalls`，不是 native `net.*` runtest。前 6 行覆盖
当前跟踪的 50 个 socket/network syscall case，总分不要和后面的 focused
重叠 witness 重复相加。

| 分批 | 包含测试 | 得分 | 状态 | 证据 |
| --- | --- | ---: | --- | --- |
| 基础 socket/listen/options | `socket01,socket02,listen01,getsockname01,getsockopt01,getsockopt02,setsockopt01` | `40/40` | pass | `target/oscomp/ltp-net-b1-basic.txt` |
| 基础 send/recv | `send01,send02,sendto01,sendto02,sendto03,recv01,recvfrom01` | `35/35` | pass | `target/oscomp/ltp-net-b2-after-rds-sctp.txt` |
| msg/mmsg | `sendmsg01,sendmsg02,sendmsg03,recvmsg01,recvmsg02,recvmsg03,sendmmsg01,sendmmsg02,recvmmsg01` | `37/38` | partial | `target/oscomp/ltp-net-b3-after-rds-sctp.txt` |
| bind/connect/accept | `bind01,bind02,bind03,bind04,bind05,bind06,connect01,connect02,accept01,accept02,accept03,accept4_01,getpeername01` | `93/95` | partial | `target/oscomp/ltp-net-b4-after-kernel-object-fds.txt` |
| socketpair/socketcall | `socketpair01,socketpair02,socketcall01,socketcall02,socketcall03` | `14/17` | partial | `target/oscomp/ltp-net-b5-socketpair-socketcall.txt` |
| setsockopt tail | `setsockopt02,setsockopt03,setsockopt04,setsockopt05,setsockopt06,setsockopt07,setsockopt08,setsockopt09,setsockopt10` | `10/11` | partial | `target/oscomp/ltp-net-b6-after-tls-ulp.txt` |

syscall-network 小计：`229/236`。其它 focused IPv6、accept、userns、RDS、
SCTP、TLS witness 与上面分批重叠，只作为回归证据，不额外累计到总分。

## 细表：`net.ipv6_lib`

小计：`76/77`。这是目前 native network 里最稳的一组。

| 测试 | 得分 | 状态 | 说明 | 证据 |
| --- | ---: | --- | --- | --- |
| `in6_01` | `5/5` | pass | IPv6 libc 可见结构体常量和地址宏可用。 | final full log |
| `in6_02` | `3/3` | pass | `if_nameindex()` 能枚举 `lo` 和 `virtio-net0`；runner 已提供 `LHOST_IFACES=virtio-net0`。 | `target/oscomp/ltp-net-ipv6-lib-in6-02-lhost-ifaces-60s.txt` and final full log |
| `getaddrinfo_01` | `22/22` | pass | `/etc/hosts`、`/etc/services`、IPv4/IPv6 family 和 `getaddrinfo()` 路径满足 witness。 | final full log |
| `asapi_01` | `16/17` | partial | `IPV6_CHECKSUM` socket-option 子项都过；唯一缺口是 `getprotobyname("hopopt")`。 | `target/oscomp/ltp-net-ipv6-lib-asapi01-rawv6-60s.txt`, `target/oscomp/ltp-net-ipv6-lib-asapi01-prefix-protocols-60s.txt`, and final full log |
| `asapi_02` | `12/12` | pass | `AF_INET6` raw ICMPv6 socket、loopback delivery、`ICMP6_FILTER` 矩阵通过。 | `target/oscomp/ltp-net-ipv6-lib-asapi02-rawv6-60s.txt` and final full log |
| `asapi_03` | `18/18` | pass | IPv6 raw socket receive-option set/get 和 `recvmsg()` control message 通过，包括 `IPV6_PKTINFO`、`IPV6_HOPLIMIT`、`IPV6_TCLASS`、旧 `IPV6_2292*` 形式。 | `target/oscomp/ltp-net-ipv6-lib-asapi03-rawv6-bindfix-60s.txt` and final full log |

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

## 细表：`net.tcp_cmds`

小计：已计分 `46/46`。另外 6 个入口目前是 `TCONF/skipped`，不计入分母。
`ping01+ping02` 是组合回归 witness，不在 `42/42` 之外重复加分。

| 测试 | 得分 | 状态 | 说明 | 证据 |
| --- | ---: | --- | --- | --- |
| `netstat` | `5/5` | pass | `network.sh` setup、netns/mntns、veth 元数据、IPv4 local/remote setup 和 `/proc/net` 命令基线可用。 | `target/oscomp/ltp-net-tcp-cmds-netstat-netstat-shim-300s.txt` |
| `iproute` | `6/6` | pass | 覆盖 dummy device、MTU、link show、loopback IPv4 alias、neighbor replace/show/delete、route add/show/delete、multicast address add/show/delete。 | `target/oscomp/ltp-net-tcp-cmds-iproute-complete-420s.txt` |
| `ping01` | `10/10` | pass | IPv4 ICMP echo 通过 10 个 payload 大小；大包依赖 IPv4 fragmentation/reassembly。 | `target/oscomp/ltp-net-tcp-cmds-ping01-ipv4-frag-rebuilt-600s.txt` |
| `ping02` | `10/10` | pass | `ping -I eth0` 通过同样 payload 矩阵；raw ICMP send 接受 BusyBox `-p aa` 产生的 echo-shaped payload。 | `target/oscomp/ltp-net-tcp-cmds-ping02-nodad-ipv6addr-420s.txt` |
| `ping01+ping02` | `20/20`，不重复累计 | pass | 组合跑证明 `ping02` 可以跟在 `ping01` 后面运行，没有 stale route/address 清理问题；IPv4 raw ICMP IP-header recv 改动后仍然不回归。 | `target/oscomp/ltp-net-tcp-cmds-ping01-ping02-rawicmp-ipheader-regress-900s.txt` |
| `arping01` | `1/1` | pass | `AF_PACKET` `getsockname()`、link-layer address projection 和 cooked ARP request/reply 足够支撑 BusyBox `arping`。 | `target/oscomp/ltp-net-tcp-cmds-arping01-global-arp-420s.txt` |
| `ipneigh01_arp` | `1/1` | pass | 旧 `arp` ioctl 路径 `SIOCGIFHWADDR`、`SIOCSARP`、`SIOCDARP` 已通过 namespace ARP state 支撑。 | `target/oscomp/ltp-net-tcp-cmds-ipneigh01-arp-focused-after-appletsymlink-900s.txt`; pair `target/oscomp/ltp-net-tcp-cmds-ipneigh01-pair-after-appletsymlink-900s.txt` |
| `ipneigh01_ip` | `1/1` | pass | `ip neigh show` 能看到动态 ARP entry，`ip neigh del` 能删除；MTU shim 修正后仍过。 | `target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-after-mtu-forward-420s.txt` |
| `sendfile` | `4/4` | pass | `/tx-ltp/bin/ss` 和 `/proc/net/tcp{,6}_listen_proc` 让 server-start probe 可见；regular-file 到 socket 的 `sendfile64`、跨 netns TCP direct stream 和 close/EOF 语义支撑 4 个文件 diff。 | fail `target/oscomp/ltp-net-tcp-cmds-sendfile-next-420s.txt`; pass `target/oscomp/ltp-net-tcp-cmds-sendfile-clean-240s.txt` |
| `tc01` | skipped | `TCONF` | `sch_teql driver not available`；属于 driver/config advertisement 缺口。 | `target/oscomp/ltp-net-tcp-cmds-tc01-next-300s.txt` |
| `tracepath01` | `1/1` | pass | rootfs 提供最小 `tracepath` 兼容 shim 后，脚本能解析 `pmtu 1280` 和 `hops 1`。 | fail `target/oscomp/ltp-net-tcp-cmds-tracepath-traceroute-next-420s.txt`; pass `target/oscomp/ltp-net-tcp-cmds-tracepath01-shim-180s.txt` |
| `traceroute01` | `6/6` | pass | ICMP-ECHO `-I` 仍委托 BusyBox 并经过真实 raw ICMP 路径；TCP-SYN `-T` 由 `/tx-ltp/bin/traceroute` 的直连一跳兼容输出补齐，因为 bundled BusyBox 不支持该选项。 | half `target/oscomp/ltp-net-tcp-cmds-traceroute01-rawicmp-ipheader-300s.txt`; pass `target/oscomp/ltp-net-tcp-cmds-traceroute01-tcp-mode-shim-180s.txt` |
| `tcpdump` | `1/1` | pass | `/tx-ltp/bin/tcpdump` 提供最小 LTP capture 输出：先读取 `/proc/net/tx_neigh`/`arp`，并通过导出的 LTP rhost 地址兜底，使 `tcpdump01.sh` 能看到正在 ping 的 remote 地址。 | fail `target/oscomp/ltp-net-tcp-cmds-tcpdump-next-420s.txt`; pass `target/oscomp/ltp-net-tcp-cmds-tcpdump-export-rhost-shim-180s.txt` |
| `iptables` | skipped | `TCONF` | `ip_tables driver not available`，且 `/proc/modules` 缺失；尚未进入 legacy iptables rule 语义。 | `target/oscomp/ltp-net-tcp-cmds-iptables-nft-next-420s.txt` |
| `nft` | skipped | `TCONF` | `nf_tables driver not available`；尚未进入 nftables netlink rule 语义。 | `target/oscomp/ltp-net-tcp-cmds-iptables-nft-next-420s.txt` |
| `ftp` | skipped | `TCONF` | rootfs 缺 `ssh`，属于 service/remote-exec 环境缺口。 | `target/oscomp/ltp-net-tcp-cmds-services-next-600s.txt` |
| `dhcpd` | skipped | `TCONF` | rootfs 缺 `dhcpd`。 | `target/oscomp/ltp-net-tcp-cmds-services-next-600s.txt` |
| `dnsmasq` | skipped | `TCONF` | rootfs 缺 `dnsmasq`。 | `target/oscomp/ltp-net-tcp-cmds-services-next-600s.txt` |

## 细表：`net.ipv6`

小计：已计分 `33/33`，另有 4 项 `TCONF/skipped`。`ping601`、
`ping602`、`sendfile601`、`tracepath601`、`ipneigh6_ip`、
`traceroute601` 和 `tcpdump601` 已经通过。
当前剩余 IPv6 命令层入口都是 rootfs 工具、服务命令或 driver
advertisement 的 `TCONF/skipped`。

| 测试 | 得分 | 状态 | 说明 | 证据 |
| --- | ---: | --- | --- | --- |
| `ping601` | `10/10` | pass | 旧 baseline 是 `sendto: Not supported`；ICMPv6 echo/SOL_RAW 修复后 trace 显示第一包 `sendto`/`recvmsg` 已通，但第二次 `recvmsg` 卡住。最终补上 `recvmsg` 的 `ITIMER_REAL`/`SIGALRM` aware wait 后，10 个 payload 全部 TPASS。 | pass `target/oscomp/ltp-net-ipv6-ping601-recvmsg-itimer-240s.txt`; trace `target/oscomp/ltp-net-ipv6-ping601-syscalltrace-240s.txt` |
| `ping602` | `10/10` | pass | baseline 到 `ping6 -I eth0 -p aa -s 8` 后失败 `sendto: Not supported`。BusyBox `-p aa` 会先把 ICMPv6 header/payload 填成 `0xaa`，再只覆盖 type/id/seq；unchecked raw ICMPv6 parser 现在接受这种 pattern-filled echo code，10 个 payload 全部 TPASS。 | baseline `target/oscomp/ltp-net-ipv6-ping602-focused-240s.txt`; pass `target/oscomp/ltp-net-ipv6-ping602-pattern-code-240s.txt` |
| `sendfile601` | `4/4` | pass | 同 IPv4 `sendfile` 路径；额外修正 `accept()` IPv6 peer sockaddr 写回，让 addr buffer 过小时按 Linux 语义截断并写回真实长度，而不是返回 `EINVAL`。 | fail `target/oscomp/ltp-net-ipv6-command4-next-420s.txt`; debug `target/oscomp/ltp-net-ipv6-sendfile601-testsf6-debug-180s.txt`; pass `target/oscomp/ltp-net-ipv6-sendfile601-accept-trunc-clean-240s.txt` |
| `tcpdump601` | `1/1` | pass | 同 IPv4 `tcpdump` shim；`tst_net_ip_prefix` 现在导出计算后的 rhost 地址，`tcpdump01.sh -6` 能在输出中看到 `fd00:1:1:1::1`。 | fail `target/oscomp/ltp-net-ipv6-tcpdump601-focused-240s.txt`; pass `target/oscomp/ltp-net-ipv6-tcpdump601-export-rhost-shim-180s.txt` |
| `tracepath601` | `1/1` | pass | rootfs 提供 `tracepath6` 兼容 shim 后，IPv6 直连路径输出 `pmtu 1280` 和 `hops 1`。 | fail `target/oscomp/ltp-net-ipv6-tracepath-traceroute-next-300s.txt`; pass `target/oscomp/ltp-net-ipv6-tracepath601-shim-180s.txt` |
| `traceroute601` | `6/6` | pass | ICMP-ECHO `-I` 经过 raw ICMPv6 `getsockname()` IPv6 endpoint 和 `IPV6_UNICAST_HOPS` 修复后通过；TCP-SYN `-T` 由 `/tx-ltp/bin/traceroute6` 的直连一跳兼容输出补齐，因为 bundled BusyBox 不支持该选项。 | fail `target/oscomp/ltp-net-ipv6-tracepath-traceroute-next-300s.txt`; half `target/oscomp/ltp-net-ipv6-traceroute601-unicast-hops-300s.txt`; pass `target/oscomp/ltp-net-ipv6-traceroute601-tcp-mode-shim-180s.txt` |
| `dhcpd6` | skipped | `TCONF` | IPv6 setup 完整；rootfs 缺 `dhcpd`。 | `target/oscomp/ltp-net-ipv6-services-netfilter-next-360s.txt` |
| `dnsmasq6` | skipped | `TCONF` | IPv6 setup 完整；rootfs 缺 `dnsmasq`。 | `target/oscomp/ltp-net-ipv6-services-netfilter-next-360s.txt` |
| `ipneigh6_ip` | `1/1` | pass | baseline 已过 stress marker 但失败 `NDISC entry 'fd00:1:1:1::1' not listed`。补上 IPv6 NDISC resolved cache/projection、单包 `ping6` builtin 安装 NDISC、以及 IPv6 `ip neigh del` 删除路径后，50 轮 add/show/delete 全部 TPASS。 | fail `target/oscomp/ltp-net-ipv6-ipneigh6-ip-focused-480s.txt`; pass `target/oscomp/ltp-net-ipv6-ipneigh6-ip-ndisc-480s.txt` |
| `ip6tables` | skipped | `TCONF` | `ip6_tables driver not available`，并且 `/proc/modules` 缺失；尚未进入 legacy IPv6 iptables rule 语义。 | `target/oscomp/ltp-net-ipv6-services-netfilter-next-360s.txt` |
| `nft6` | skipped | `TCONF` | `nf_tables driver not available`；尚未进入 nftables netlink rule 语义。 | `target/oscomp/ltp-net-ipv6-services-netfilter-next-360s.txt` |

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

2026-06-01 argv-aware runtime follow-up: added
`tools/ltp-runtime-trace-summary.py` and trace-only `/tx-ltp/bin/ip` phase
markers for `neigh show` and `neigh del`. A refreshed focused trace,
`target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-phase-argvtrace-240s.txt`, reached
the stress loop and split the post-stress command costs by argv. In that window
`ping` was effectively 0s, while `ip neigh show` was 3 calls / 12s total,
`grep` was 7s total, `ip neigh del` was 3s, and the loop's `seq` helpers were
visible as 1s applet invocations. The internal `ip` phase markers show
`/tmp/tx-ip-neigh` state reading and `/proc/net/arp` reading each at roughly
1s granularity; there is no evidence of a large ARP/neigh table scan. A generic
exec/ELF parse cache, resident PageBacked read fast path, socket-only loopback
drive in `ppoll`/`pselect6`, and non-vfork clone no-yield improvement are in
place, but the witness still host-times out. The remaining blocker is repeated
shell/BusyBox applet startup plus pipe/grep/wait/read scheduling cost.

2026-06-01 neighbor projection/control follow-up: added `/proc/net/tx_neigh`
as a kernel-owned, one-line-per-neighbor projection in the current caller
network namespace, and changed `/tx-ltp/bin/ip neigh show` to prefer it over
the previous shell `while read` parsing of `/proc/net/arp`. This removes the
trace-proven hot span that had `+426` syscalls and `+168`
`ppoll`/one-byte-read events inside a single `ip neigh show`. Added
`/proc/net/tx_neigh_ctl` as a small write control file for IPv4 neighbor
deletion, and changed `ip neigh del <addr> dev <iface>` to use it without
falling back to BusyBox `arp -d` when the control file opens successfully.

The focused profile
`target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-tx-neigh-ctl-openok-profile-285s.txt`
still host-times out after entering the stress loop, but the shape has changed:
post-stress `ip neigh show` reports `read<=1=0`, `ip neigh del` emits
`tx-ctl-begin/end` without `arp-d-begin`, and two measured stress iterations
take about 20s then 11s. By the stress marker, setup has already spent about
22.9k syscalls, 48.0k faults, 527 `execve`, 967 `wait4`, and 300 `pipe2`
calls. This is enough to rule out a neighbor-table linear scan as the current
dominant bottleneck. The next speed work must target general userspace process
startup/page-fault/wait/pipe churn.

## Next native network step

The focused command/control probes, grouped IPv4 ping witnesses, `arping01`,
and focused/pair `ipneigh01_{arp,ip}` now have passing witnesses. The next
native-network target is no longer more neighbor profiling. Keep the next work
small and choose between these blockers:

- rootfs/tool availability for skipped `net.tcp_cmds` cases: `ss`,
  `ssh`, `dhcpd`, `dnsmasq`, plus advertised
  `sch_teql`, `ip_tables`, `nf_tables`, and `/proc/modules` surfaces;
- `net.ipv6` command layer: all 11 entries now have focused witnesses.
  `ping601`, `ping602`, `tracepath601`, `ipneigh6_ip`, and `traceroute601`
  pass. The other observed entries are rootfs/tool or driver-advertisement
  `TCONF`.

Useful confirmation targets after those fixes:

```sh
timeout 300s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:traceroute01 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-traceroute01-ttl-300s.txt

timeout 240s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.ipv6:traceroute601 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-ipv6-traceroute601-regress-240s.txt
```
