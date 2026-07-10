# IPv6 支持现状与补全方案

<!-- txdoc:07-NET-IPV6-STATUS-V1 -->

> 分支 `feature-network-refactor`(P0–P4 已落)。回答三件事:① IPv6 现在**实现到哪一步**;② IPv6 与 IPv4 在这套栈里**什么关系**(哪些共享、哪些各写一套);③ 要**完全实现 IPv6** 差在哪、怎么补。证据均带 `file:line`。配套:审计 [`NET_AUDIT_v1.md`](NET_AUDIT_v1.md) §3-bis(`txdoc:07-NET-AUDIT-IPV6`)。

---

## 0. 结论速览

**当前 IPv6 = "loopback 通 + 收包通,外部发包死、路由/邻居缺"。** 比审计 main 快照(§3-bis 的"有壳无数据路径")已明显前进——P0–P4 把 v6 的 **loopback 数据路径**和 **RX 分用**自研跑通了;剩下的黑洞集中在**外部(真网卡)发包的 L3/L2 转发面 + 路由 + 动态邻居**。

| 能力 | 状态 | 一句话 |
|---|---|---|
| v6 地址/类型/socket/双栈 bind | ✅ 通 | 类型、`sockaddr_in6`、`[::]`→v4 映射都在 |
| **v6 loopback TCP/UDP** | ✅ **端到端通** | 重构新增,家族无关 |
| **v6 RX 分用(TCP/UDP)** | ✅ 通 | `demux_ipv6`→L4 |
| v6 RX ICMPv6 分用 | ❌ 丢 | demux 返 Unsupported |
| **v6 外部 TX(TCP/UDP/raw)** | ❌ **死** | L3 dispatch/L2 帧/路由全 v4-only |
| **v6 路由/FIB** | ❌ stub | 路由结构只存 `Ipv4Address` |
| **v6 动态邻居 NDP** | ❌ 只静态 | 无 NS/NA/RS/RA、无 RX 学习 |
| v6 分片/重组 | ❌ 缺 | P4 显式延后 |
| v6 转发控制 / `/proc/net/ipv6_route` | ❌ 缺 | 无 `ipv6_forwarding` 字段 |
| 控制面(procfs `if_inet6`/邻居渲染/raw-icmp6) | ✅ 通 | ping6/tracepath6 靠这个复活 |

---

## 1. IPv4 与 IPv6 的关系:哪里共享、哪里各写一套

这套栈的根因(审计 §1/§4)是 **把 smoltcp 降格成 wire 编解码库、没有持久 `Interface`**。后果:凡是 smoltcp 的 wire 层能干的,v4/v6 **共享**;凡是需要"接口/转发面"逻辑的(路由、邻居、分片、L2 帧、设备 TX),都得**自研**,而自研目前只把 IPv4 那套写全了,v6 是二等公民。

```
        ┌─────────────── 共享(wire 层,v4/v6 同一份)───────────────┐
socket 类型/地址   IpEndpoint(family 字段同载 v4/v6, types.rs:255)
wire 编解码        emit_ipv4_packet 收 IpRepr(可 v4 可 v6)、emit_v6(udp.rs:534)
                  parse fallback(先 v4 再试 v6, udp.rs:457)
RX 分用           demux_ipv6(ether/mod.rs:303)→ demux_tcp_v6/demux_udp
loopback          is_loopback_destination 查两族(step_udp_loopback.rs:145)
        └──────────────────────────────────────────────────────────┘
                              │
        ┌──────── 分叉(转发面,IPv4 写全 / IPv6 stub)──────────────┐
路由/FIB          decide_ipv4_route(ether/mod.rs:906) │ 无 decide_ipv6_route
外部 L3 dispatch   dispatch_ip_at 只解 Ipv4Packet(:316) │ 无 dispatch_ipv6_at
L2 以太帧          build_ipv4_ethernet_frame(:664)     │ 无 build_ipv6_ethernet_frame
邻居              ARP 动态学习(link.rs process_arp)   │ NDP 只静态表(ndisc_table)
分片/重组          ipv4_fragments 表(mod.rs:130)       │ 无 v6 重组(:301 注释延后)
        └──────────────────────────────────────────────────────────┘
```

**一句话**:v4/v6 **共享 wire + loopback + 收包**,**分叉在"外部发包的整条转发面"**——路由→邻居→L2 帧→设备 TX,这条链 v6 全是空的。**类型层还有个隐患**(审计:153):`IpEndpoint` 靠 `family` 字段约定同载 v4/v6,类型层挡不住"family=Inet 却读 addr6";彻底修要 `IpEndpoint→enum{V4,V6}`。

---

## 2. 现状全景(分层 · 数据面 + 控制面)

### 2a. 数据面(wire/datapath)

| 层 | 状态 | 证据 | 备注 |
|---|---|---|---|
| UDP v6 emit/parse | ✅ | `udp.rs:534 emit_v6` / `:457 parse_ipv6` | 真 `Ipv6Repr` |
| TCP v6 emit/parse | ✅ | `tcp.rs:525`(emit 收 IpRepr)/`:556` | |
| UDP/TCP v6 **loopback** | ✅ 端到端 | `step_udp_loopback.rs:145,227` / `step_tcp_loopback.rs:72` | |
| v6 RX 分用 TCP/UDP | ✅ | `ether/mod.rs:303 demux_ipv6`;`smoltcp_demux.rs:43 demux_tcp_v6`/`:137` | 含校验和 |
| v6 RX **ICMPv6** 分用 | ❌ 丢 | `smoltcp_demux.rs:34-40`(Icmpv6→Unsupported) | echo/ND/RA 全被丢 |
| **外部 TX**(TCP/UDP) | ❌ stub | `step_device_tx.rs:254,322`→`dispatch_ip_at`(`ether/mod.rs:316` 只解 Ipv4) | v6 包到这就 EINVAL |
| raw socket v6 TX | ⚠️ 半 | `step_send.rs:589 send_raw_ipv6`(loopback 通 `:627`,外部 `:623` 拒) | 仅 loopback+echo |
| **L3 v6 dispatch(TX)** | ❌ 缺 | `ether/mod.rs:308 dispatch_ip_at` 无 v6 臂 | |
| **L2 v6 以太帧** | ❌ 缺 | `ether/mod.rs:664` 仅 `build_ipv4_ethernet_frame` | 即便 L3 通也发不出 |
| ICMPv6 echo build | ✅ | `icmp.rs:511 build_icmpv6_echo_message` | 仅 loopback/raw 上下文 |
| **动态 NDP**(NS/NA/RS/RA) | ❌ 缺 | `ether/mod.rs:129 ndisc_table` + `install_static_ndisc`(:428) | 只静态、无 RX 学习 |
| v6 分片/重组 | ❌ 缺 | `ether/mod.rs:301` 注释("v6 fragmentation is a P4 concern") | |

### 2b. 控制面(addressing/routing/procfs)

| 层 | 状态 | 证据 |
|---|---|---|
| v6 地址类型 / `SockAddrIn6` | ✅ | `types.rs:188-216, 404-424` |
| socket 双栈 bind/connect(`[::]`→v4) | ✅ | `table.rs:486, 578` |
| 每 iface v6 地址增删/查 | ✅ | `namespace.rs:1335 add/del/set_device_ipv6_addr_by_ifindex`;`owns_ipv6_addr`(:718) |
| v6 源地址选择 | ⚠️ | `step_send.rs:765 preferred_ipv6_source_for` 有,但 **UDP 路没调**(`payload.rs:1253` v6 返 None) |
| **v6 路由** | ❌ stub | `namespace.rs:161`(路由结构只存 `Ipv4Address`);无 `best_ipv6_route` |
| **v6 路由 rtnetlink** | ❌ stub | `rtnetlink.rs:1421`(路由消息硬编 `AF_INET`) |
| **v6 转发控制** | ❌ 缺 | 只有 `ipv4_forwarding`(`namespace.rs:65`) |
| **`/proc/net/ipv6_route`** | ❌ 缺 | 只有 v4 `/proc/net/route` |
| procfs `if_inet6` / 邻居渲染 | ✅ | `procfs/read.rs:97` / `project.rs:82` |
| raw ICMPv6 socket + `icmp6_filter` | ✅ | `payload.rs:985` / 投递 `step_send.rs:712` |
| sysctl `net.ipv6.conf.*` | ⚠️ | `procfs/read.rs:48`(disable_ipv6/accept_dad 读硬编 "0",写 no-op) |

---

## 3. 差距清单(按"阻塞面"排序)

1. **外部 L3 发包链缺失(最大)**:`dispatch_ipv6_at` + `build_ipv6_ethernet_frame` + `decide_ipv6_route` 三件全无 → **所有非 loopback 的 v6 TCP/UDP/raw 都发不出**。这是"外部 v6 完全不通"的总闸。
2. **v6 路由/FIB stub**:路由表结构只存 `Ipv4Address`,无 `best_ipv6_route`;rtnetlink 硬编 `AF_INET`;无 `/proc/net/ipv6_route` → `ip -6 route` 空、无网关解析。
3. **动态 NDP 缺**:只有静态 `ndisc_table`,无 NS/NA(邻居请求/应答)、RS/RA、无 RX 学习 → 外部 v6 无法解析下一跳 MAC。
4. **ICMPv6 RX 分用丢**:`smoltcp_demux` 把 Icmpv6 当 Unsupported → 外部 ping6/ND/RA 收不进来。
5. **v6 分片/重组缺**:大 v6 报文 TX 不分片、RX 不重组。
6. **零碎**:UDP v6 源地址 hint 缺(`payload.rs:1253`)、`ipv6_forwarding` 无字段、sysctl v6 写 no-op。

---

## 4. 补全方案:两条战略路线

审计 §3-bis 的核心洞见:**别把 IPv6 当"第二套自研栈"再抄一遍**——那正是当前困境的来源。有两条路:

### A 路 —— 持久 smoltcp `Interface`(审计推荐,"v6 顺带复活")

上一个真正的 `smoltcp::iface::Interface`,让 smoltcp **统一承载** v4/v6 的 **邻居发现(ARP+NDP)/路由/分片/RX 分用/TX dispatch**。IPv6 几乎不用单独写——smoltcp 内置全套。
- **收益**:§3 里 1/3/4/5 差距**一次性消失**(NDP、路由、分片、ICMPv6 都是 smoltcp 内置);v4 也一并简化。
- **代价**:是**重架构**(审计的"方案 A")——要把现在自研的 loopback/demux/step 收发**改接到 Interface 的 poll 模型**,丢掉一部分已跑通的自研代码;风险高、投入大,要全栈回归。

### B 路 —— 增量自研,把 v6 转发面补齐(照 v4 mirror)

保留当前 smoltcp-as-wire 架构,顺着 §2a 的分叉点,把 v6 缺的每块**照 IPv4 的样子写一遍**:
- `dispatch_ipv6_at`(mirror `dispatch_ip_at`)+ `build_ipv6_ethernet_frame`(mirror v4)+ `decide_ipv6_route`;
- v6 路由结构从 `Ipv4Address` 泛化到 `IpAddress`(或加 v6 路由表)+ rtnetlink v6 序列化 + `/proc/net/ipv6_route`;
- 动态 NDP:`ether/link.rs` 加 NS/NA 收发 + RX 学习(mirror `process_arp`/`learn_arp`);
- `smoltcp_demux` 加 ICMPv6 臂(NS/NA/echo/RA);
- v6 分片/重组(mirror `ipv4_fragments`);UDP v6 src hint 接上 `preferred_ipv6_source_for`。
- **收益**:增量、每步可验、保留已跑通的 v6 loopback/demux;风险可控。
- **代价**:**代码更多**(等于把 v4 转发面复制一份到 v6),正是审计警告的"第二套";未来维护双份。

### 该选哪条?

**你当前分支重投在 B 架构上**(loopback/demux/step 都是自研且已跑通)——现在切 A = 推翻重来。务实建议:
- **若目标是"现场赛/LTP 把 v6 靶跑绿"**:走 **B 增量**,优先补 §3 的 #1(外部 L3 链)让外部 v6 通,再按 LTP 靶补 #2-#4。回报快、风险低。
- **若目标是"长期架构正确、砍掉 v4/v6 双份"**:走 **A**,但当独立大重构立项,别混进现在的瘦身/收尾。

---

## 5. 推荐分阶段(B 路,针对当前架构)

| 阶段 | 内容 | 解锁 | 验证门 |
|---|---|---|---|
| V1 | `dispatch_ipv6_at` + `build_ipv6_ethernet_frame` + ICMPv6 RX demux 臂 | 外部 v6 收发帧成形 | v6 loopback 不回归 + 抓包见 v6 帧 |
| V2 | 动态 NDP(NS/NA + RX 学习,mirror ARP) | 外部 v6 下一跳解析 | ping6 外部网关通 |
| V3 | v6 路由/FIB 泛化 + `decide_ipv6_route` + `/proc/net/ipv6_route` | `ip -6 route`、外部 v6 TCP/UDP | LTP `net.ipv6` 路由类 + 外部 v6 iperf |
| V4 | v6 分片/重组 + UDP v6 src hint + `ipv6_forwarding` + sysctl 真值 | 大 v6 报文、转发、sysctl 一致性 | LTP v6 分片/forwarding 靶 |

每阶段独立可验、可回退。**硬门槛**:每步都跑 `cargo test -p tx-subsystems` 集合差=基线 + v6 loopback 不回归(现在是唯一跑通的 v6 数据路径,别改坏)。

---

## 6. 验证靶(现在能拿的 v6 分)

- 已有:`net.ipv6` 的 ping601/602、tracepath601、tcpdump601(靠已恢复的控制面 + ICMPv6 loopback)。
- 补 #1-#3 后可争取:外部 ping6、`ip -6 addr/route`、getaddrinfo v6、v6 iperf/netperf(若靶用 v6)。
- 注意(审计:201):历史 `feature-network-next` 有过 v6 procfs/ping6 工作但**未入 main**;本分支已自研恢复了更多,但**外部数据面短板仍在**——本文即是复核结果。

---

## 7. V1 已落地(2026-07-10)

§5 表的 **V1 行**已实现并过回归门。

**V1a 对外 v6 L3 发送**(镜像 v4 `dispatch_ip_at` 链):
- `types.rs`:`Ipv6Address::is_multicast`(`ff00::/8`)
- `loopback.rs`:`IfaceCommon` 加 `ipv6_addr`/`ipv6_prefix_len` + `with_ipv6`/getter
- `namespace.rs`:v6 地址/前缀从 `NetNamespaceLinkInfo` 串到 `EtherIface`,并纳入 iface 缓存键
- `ether/mod.rs`:`dispatch_ipv6_at`→`decide_ipv6_route`(multicast/同前缀→Direct,否则 Unreachable)→`resolve_ndisc`(静态 `ndisc_table` + `multicast_mac_for`=33:33+低4字节;未命中 EADDRNOTAVAIL)→`transmit_ipv6_packet`(≤MTU `build_ipv6_ethernet_frame`+`transmit_frame`,超则 EMSGSIZE)

**V1b ICMPv6 接收 demux**:
- `demux.rs`:`PacketDispatch::Icmp6(RawIpv6Packet)`
- `smoltcp_demux.rs`:`demux_ipv6` 的 `IpProtocol::Icmpv6` 臂 → `RawIpv6Packet{src,dst,next_header=58,payload}`
- `step_send.rs`:`deliver_raw_ipv6_packet_to_table` 提为 `pub(crate)` 收 `&SocketTable`(**发送/接收共用同一分发器**),2 caller 改传 `payload.socket_table()`
- `step_process_network_events.rs`:`Icmp6` 臂把回包扇出到匹配 raw-icmp6 socket(family=Inet6/protocol/bound_local6/icmp6_filter 过滤,`fire_recv` 唤醒)

合起 = 对外 ping6 的 **TX(echo request 上线)+ RX(echo reply 落 raw socket)** 两半;邻居走静态 `ndisc_table`(同 v4 静态 ARP,动态 NDP = V2)。

**验证**:`cargo xtask build --target rv64-qemu` 干净;`cargo test -p tx-subsystems` 集合差 **313==313 零回归**(v6 loopback 无退化);0 新增 warning。**功能门(QEMU 外部 ping6)未跑**——host net 套被 1.94 污染基线掩盖,端到端需真机 + 预置静态邻居。

**下一步**:V2 动态 NDP(NS/NA 收发 + 邻居学习,替静态 `ndisc_table`),先出调研文档再实现。

---

## 8. V2 已落地(2026-07-10)

§5 表 **V2 行**(动态 NDP)已实现并过回归门。正本计划 [[IPV6_V2_PLAN_v1]]。**复用** smoltcp-asterinas 的 `NdiscRepr` / `Icmpv6Repr::Ndisc`(NS/NA parse+emit),不手搓 wire。

- **RX**(`ether/link.rs` `maybe_process_ndisc`,side-effect peek 镜像 `maybe_reply_icmpv4`,挂 RX 的 Ipv6 臂):学 NS/NA 邻居(NS 源 lladdr / NA 目标 lladdr → `ndisc_table`),对目标为本机的 NS 回单播 NA。
- **TX**(`resolve_ndisc` 动态化):multicast→MAC;unicast 命中 TTL 缓存→Resolved;miss→`queue_pending_ndisc` 返回 `Pending`(`dispatch_ipv6_at` 转 `PendingResolution`,上层重传)。
- **探测**(`flush_pending_ndisc_at`,`step_flush_pending_arp` 一个 tick 同驱 v4+v6):对 pending 发 NS 到 solicited-node 组播(`33:33:ff:..`),退避 retry `NDISC_SOLICIT_RETRY_LIMIT=3`,超限标 `EADDRNOTAVAIL`。
- **计划外必需修正**:`accepts_ethernet_destination` 增收本机 solicited-node 组播 MAC——否则入站 NS 被丢、回 NA 永不触发。

**验证**:rv64 build 干净;`ether_iface_arp_tests` 4 个 ndisc 单测隔离 4/4 ok(NA 学习 / NS-for-us 学+回 NA / miss→NS→NA 解析 / retry-limit→Failed);全量集合差唯一新增 = 这 4 个新测试(host-harness 全量级联,隔离全过),**0 既有回归**。功能门(QEMU 外部 ping6)需真机。

**下一步**:V3(v6 路由/FIB 泛化 + `/proc/net/ipv6_route`)先出调研文档。

---

## 9. V3a 已落地(2026-07-10)

§5 表 **V3 行的管理面**(v6 FIB + rtnetlink v6 route + `/proc/net/ipv6_route`)已实现。正本计划 [[IPV6_V3_PLAN_v1]]。**调研发现**:外部 v6 TCP/UDP 还需数据面(`emit_ipv6`,socket→wire 也纯 v4),故 V3 拆 **V3a(管理面,本节)+ V3b(数据面,待做)**。

- **v6 FIB**(namespace.rs,独立 `routes6` 表镜像 v4):`NetNamespaceRoute6{Info,Config,Selector,Decision,Entry}` + `route6_snapshot`(连接路由从 iface v6 地址合成 + 显式 + 最长前缀)+ `best_ipv6_route` + `add`/`delete_ipv6_route`;v6 前缀数学用 u128 大端掩码(`plen==0` 守卫)。
- **rtnetlink v6 route**:`handle_newroute/delroute` 按 `rtmsg.family` 分流(AF_INET6→v6,v4 分支原样)+ `parse_route6_config/selector` + `ipv6_attr` + `render_getroute_dump` 按 family 门控 v6 + `build_route6_message`。
- **`/proc/net/ipv6_route`**:`proc_net_ipv6_route_snapshot_text`(Linux 格式:dst/src/nexthop 各 32hex + prefixlen + flags UP|GATEWAY)+ tx-fs procfs 布线(`PROCFS_NET_IPV6_ROUTE_ID`)。

**验证**:rv64 build 干净;`rtnetlink_tests` 隔离 24/24(既有 v4 route + 新 v6 route 同过);全量集合差唯一新增=3 个新 v6 route 测试(级联,隔离过),**0 既有回归**(v4 路由/数据面未动)。功能门(QEMU `ip -6 route`)需真机。

**下一步**:**V3b 数据面**(`emit_ipv6_packet` + step_device_tx v6 分支 + step_send/connect v6 选路 + `decide_ipv6_route` 网关)——解锁外部 off-link v6 TCP/UDP 真流量,先出调研文档。

---

## 10. V3b 已落地(2026-07-10)

§5 表 **V3 行的数据面**(外部 off-link v6 TCP/UDP 路由)已实现。正本计划 [[IPV6_V3B_PLAN_v1]]。**调研发现**:emit / 源选择 / on-link 路由**早已就绪**(`emit_ipv4_packet` 双族、UDP `local` 双族、connect 源选择双族、on-link→Direct→NDP),真缺口仅 off-link 网关,故 V3b ~100 LOC(远小于 [[IPV6_V3_PLAN_v1]] §4 估计)。

- **`IfaceCommon.ipv6_gateway`**(loopback.rs):`with_ipv6_gateway` 链式 setter + getter(V1a 注释早留坑)。
- **`decide_ipv6_route` 网关分支**(ether/mod.rs):off-link + 有 v6 网关 → `Ipv6RouteDecision::Gateway{next_hop}`;`dispatch_ipv6_at` 零改(网关 next_hop 经 `resolve_ndisc` 走 NDP 解析)。
- **v6 网关灌入**(namespace.rs):`gateway6_for_device`(从 `routes6` ::/0 默认路由解析,镜像 v4 `gateway_for_device`)+ `NetNamespaceIfaceRuntime.ipv6_gateway` + `ensure_ether_iface_for_link` 灌入 + 缓存键。

**验证**:rv64 build 干净;2 测试隔离全 ok(`decide_ipv6_route` 四路径 + off-link 排网关不排 dst);decide 纯函数测试连全量都过;集合差新增失败仅 off-link 集成测试(级联,隔离过),**0 既有回归**。**边界**:socket→emit_v6 段代码复核双族、未写 socket 级端到端 v6 UDP 测试;功能门(QEMU 外部 v6 iperf)需真机。

**结果**:外部 off-link v6 TCP/UDP 收发链内核侧齐了 = 路由(V3b)+ emit 双族 + 源选择双族 + NDP(V2)。

**下一步**:V4(v6 分片/重组 + `ipv6_forwarding` + sysctl 真值),或按 LTP v6 靶优先。

---

## 11. V4 延后 + B 路收官(2026-07-10)

**V4 已延后**(用户确认)。调研正本 [[IPV6_V4_PLAN_v1]]:V4(v6 分片 ~200 + `ipv6_forwarding` ~100 + sysctl ~30)是架构完整性收尾,**已知 LTP 计分价值近零**——账本无 v6 分片/转发靶,可计分 v6 靶(ipv6_lib/ping6/tracepath601/tcpdump601)V1-V2 已覆盖,v6 分片实务罕见。§5 表列进 V4 的 **UDP v6 源 hint 实为已就绪**(`preferred_ipv6_source_for`)。触发条件 + 实现草图见 [[IPV6_V4_PLAN_v1]] §5/§6。

**IPv6 B 路 V1-V3b 收官**——补齐审计⑩「v6 有壳无数据路径」的核心,外部 off-link v6 TCP/UDP 收发链内核侧已齐:

| 阶段 | 提交 | 能力 |
|---|---|---|
| V1 | a90988d0 | 对外 v6 L3 发送 + ICMPv6 RX demux |
| V2 | ccae95c8 | 动态 NDP 邻居发现 |
| V3a | 6ee95e0b | v6 路由/FIB 管理面(`ip -6 route` + `/proc/net/ipv6_route`) |
| V3b | 28a05d02 | 外部 off-link v6 路由(`decide_ipv6_route` 网关) |
| V4 | — | 分片/转发/sysctl:**延后**(LTP 价值近零) |

**未跑的功能门**:QEMU 真机外部 v6(ping6/iperf)——V1-V3b 均为回归门(集合差 0)+ 隔离单测验证,端到端真机验证待做。
