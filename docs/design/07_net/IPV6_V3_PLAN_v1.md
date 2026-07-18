<!-- txdoc:07-NET-IPV6-V3-PLAN-V1 -->

# IPv6 V3 实现计划:路由 / FIB

> 前置:[[IPV6_STATUS_v1]] §5 表 V3 行、已提交 V1(a90988d0)+ V2(ccae95c8)。
> 本文是 **V3 实现前的调研稿**,待审查后再动手(遵循每阶段"先调研→出文档→审查→实现")。

---

## 1. 目标与本次调研的关键发现

§5 表把 V3 定为"v6 路由/FIB 泛化 + `/proc/net/ipv6_route`",解锁 **`ip -6 route`** 与 **外部 v6 TCP/UDP**。

**调研的关键发现(改变了范围判断)**:"外部 v6 TCP/UDP"不是单靠路由表就能解锁的——**内核的 socket→wire 数据路径目前也是纯 v4**(`emit_ipv4_packet` 唯一,无 v6 变体)。所以 V3 实际横跨**两个都还是纯 v4 的平面**:

1. **管理面(FIB/路由表)**:namespace `routes: Vec<NetNamespaceRouteEntry>` + `best_ipv4_route` + rtnetlink newroute/getroute + `/proc/net/route`——**全 v4**。
2. **数据面(socket→wire 发包)**:`emit_ipv4_packet`(tcp.rs:525 / udp.rs:494)+ step_device_tx(TCP :254 / UDP :322 都发 v4)+ step_send/step_connect 用 `best_ipv4_route` 选路——**全 v4**。

结论:**V3 应拆成两个可独立审查/验证的子阶段**(类比 V1a/V1b):
- **V3a = 管理面**(v6 FIB + rtnetlink v6 route + `/proc/net/ipv6_route`)→ 解锁 `ip -6 route add/del/show`。自足、镜像 v4、低风险。
- **V3b = 数据面**(`emit_ipv6_packet` + step_device_tx v6 分支 + step_send/connect v6 选路 + `decide_ipv6_route` 网关)→ 解锁**外部 off-link v6 TCP/UDP 真流量**。依赖 V3a(FIB 给 egress/网关)+ V2(NDP 解析网关 MAC)。

**推荐:先做 V3a**(自足、镜像 v4、解锁 `ip -6 route` + LTP v6 路由管理靶),V3b 作为单独审查的后续阶段。理由见 §8。

**非目标(→V4)**:v6 分片/重组、UDP v6 源地址 hint、`ipv6_forwarding` 转发、sysctl 真值。

---

## 2. 当前 v4 路由架构(要 mirror 的蓝图)

### 2.1 管理面(FIB)

- **存储**:`NetNamespacePayload.routes: SpinMutex<Vec<NetNamespaceRouteEntry>>`(namespace.rs:73);`NetNamespaceRouteEntry/Info/Config/Selector/Decision` 全 `Ipv4Address` 字段(:161-238)。
- **快照**:`route_snapshot()`(namespace.rs:~830)= 自动生成**连接路由**(从每个 up iface 的 `ipv4_addr/prefix` 合成 `dst=子网, oif=iface, preferred_src=addr, scope=253`)+ 追加显式 `routes` + 按**最长前缀**排序。
- **查询**:`best_ipv4_route(dst)`(:984)= 快照→`route_matches_ipv4` 过滤→`route_decision_for_info`(解析 oif、校验 link up+有 v4 地址)→`max_by_key(prefix_len)`。返回 `NetNamespaceRouteDecision{oif_name, next_hop=gateway.unwrap_or(dst), preferred_src, prefix_len, kind}`。
- **增删**:`add_ipv4_route`/`delete_ipv4_route`(:869/896,校验+去重+按 selector 删)。
- **rtnetlink**:`handle_newroute`(rtnetlink.rs:1077)→`parse_route_config`→`add_ipv4_route`;`handle_delroute`→`delete_ipv4_route`;`render_getroute_dump`(:792)转储;`build_route_message`(:1413)。
- **`/proc/net/route`**:`proc_net_route_snapshot_text`(project.rs:153)= Linux 格式(`Iface\tDestination\tGateway\tFlags\t…`,dst/gw/mask 都 8 位大端 hex)。

### 2.2 数据面(socket→wire)

- **选路**:socket connect/send 在 **step_send.rs:827 / step_connect.rs:785** 调 `best_ipv4_route(dst)` 拿 egress+next_hop+preferred_src。
- **成包**:`SmoltcpTcpSegment::emit_ipv4_packet`(tcp.rs:525)、`UdpTxDatagram::emit_ipv4_packet`(udp.rs:494)——**只有 v4**。
- **发送**:`step_process_device_tx_pending_in_namespace_at`(step_device_tx.rs:118)遍历 TCP/UDP/raw socket,`dispatch_segment().emit_ipv4_packet()` → `sink.transmit_at(packet)` → iface `dispatch_ip_at` → **版本 nibble 分流**(V1a 装的)→ v4 `decide_ipv4_route` / v6 `dispatch_ipv6_at`。
- **iface 决策**:`decide_ipv4_route(common, dst)`(纯函数,连接子网→Direct / 单网关→Gateway / 否则 Unreachable)。

### 2.3 v6 现状(缺口)

| 部件 | v4 | v6 现状 |
|---|---|---|
| FIB 存储 | `routes` Vec | ❌ 无 |
| FIB 查询 | `best_ipv4_route` | ❌ 无 |
| iface 决策 | `decide_ipv4_route`(连接+网关) | ⚠️ `decide_ipv6_route`(**仅 on-link**,V1a;无网关、无 FIB) |
| rtnetlink route | newroute/delroute/getroute | ❌ `handle_newroute` 硬调 `add_ipv4_route` |
| `/proc/net/*route*` | `/proc/net/route` | ❌ 无 `/proc/net/ipv6_route` |
| 成包 | `emit_ipv4_packet` | ❌ 无 `emit_ipv6_packet` |
| device_tx 发送 | v4 分支 | ❌ 无 v6 分支 |
| socket 选路 | step_send/connect 调 best_ipv4_route | ❌ 无 v6 选路 |

---

## 3. V3a — 管理面(v6 FIB + rtnetlink + /proc)

**目标**:`ip -6 route add/del/show`、`cat /proc/net/ipv6_route` 工作;FIB 存 v6 路由供 V3b 查询。**不触碰数据面**,因此不改变任何现有流量行为。

### 3.1 设计(决策 D1:独立 v6 路由表,不 enum 化 v4 结构)

新增**平行的 v6 路由表**(镜像 ARP→NDP 的"独立表"模式,而非把 `Ipv4Address` 字段 enum 化——后者会波及 `/proc/net/route`、rtnetlink v4、所有消费者):

```rust
// namespace.rs —— 平行 v4 结构,字段换 Ipv6Address
struct NetNamespaceRoute6Entry { dst: Ipv6Address, prefix_len: u8, gateway: Option<Ipv6Address>,
    oif_name: Option<&'static str>, preferred_src: Option<Ipv6Address>, table: u8, protocol: u8, scope: u8, route_type: u8 }
pub struct NetNamespaceRoute6Info { … }      // + Config / Selector / Decision 平行体
// NetNamespacePayload 加字段:
routes6: SpinMutex<Vec<NetNamespaceRoute6Entry>>,
suppressed_connected_routes6: SpinMutex<Vec<…>>,   // 若需连接路由抑制
```

方法(逐一镜像 v4):`route6_snapshot`(连接路由从 iface `ipv6_addr/prefix` 合成 + 追加 `routes6` + 最长前缀排序)、`best_ipv6_route(dst)`、`route6_decision_for_info`、`add_ipv6_route`、`delete_ipv6_route`。

### 3.2 rtnetlink v6 route

- `handle_newroute`/`handle_delroute`(rtnetlink.rs:1077/1083):按 `rtmsg.family` 分流——`AF_INET`→现有 v4 路径,`AF_INET6`→新 `parse_route6_config`→`add_ipv6_route`。
- `render_getroute_dump`(:792):追加 v6 路由转储(family=AF_INET6,`build_route6_message`,RTA_DST/GATEWAY/OIF/PREFSRC 用 16 字节地址)。
- `parse_route_config` 已部分识别 AF_INET6(:986/1020 有分支)——复核后接上 v6 存储。

### 3.3 `/proc/net/ipv6_route`

新增 `proc_net_ipv6_route_snapshot_text`(project.rs,mirror :153)。**注意 Linux v6 格式与 v4 不同**:每行 = `dst(32hex) prefixlen(2hex) src(32hex) srcplen(2hex) nexthop(32hex) metric(8hex) refcnt use flags(8hex) dev`(无表头)。挂到 procfs `/proc/net/ipv6_route`(找 v4 `/proc/net/route` 的挂载点平行加)。

### 3.4 V3a 文件计划

| 文件 | 改动 | 量级 |
|---|---|---|
| `namespace.rs` | `NetNamespaceRoute6{Entry,Info,Config,Selector,Decision}` + `routes6` 字段 + `route6_snapshot`/`best_ipv6_route`/`route6_decision_for_info`/`add_ipv6_route`/`delete_ipv6_route` | 中(~250,照抄 v4) |
| `rtnetlink.rs` | newroute/delroute 按 family 分流 + `parse_route6_config` + `render_getroute_dump` v6 + `build_route6_message` | 中(~180) |
| `project.rs` | `proc_net_ipv6_route_snapshot_text`(Linux v6 格式) | 小(~40) |
| procfs 挂载点 | 注册 `/proc/net/ipv6_route` | 小 |
| `tests/rtnetlink_tests.rs` | v6 newroute/getroute/delroute round-trip(mirror 现有 v4 route 测试) | 中 |

---

## 4. V3b — 数据面(外部 v6 TCP/UDP 真流量)

**目标**:外部 off-link v6 TCP/UDP 实际收发(iperf6/netperf6)。**依赖 V3a**(FIB 给 egress+网关)+ **V2**(NDP 解析网关 MAC)。

### 4.1 设计

1. **成包**:`SmoltcpTcpSegment::emit_ipv6_packet`(tcp.rs)、`UdpTxDatagram::emit_ipv6_packet`(udp.rs)——smoltcp 段/数据报解析器 P1 起已是双族,`emit` 侧补 v6(`IpRepr::Ipv6(Ipv6Repr{…})`,同 V2 `build_ndisc_frame` 套路)。
2. **device_tx v6 分支**:`step_device_tx.rs` 的 TCP/UDP/raw 发包按 socket family(`SocketProtocol`/`IpEndpoint.family`)选 `emit_ipv4_packet` 或 `emit_ipv6_packet`。
3. **socket 选路**:step_send/step_connect 对 v6 dst 调 `best_ipv6_route`(V3a 提供)拿 egress+next_hop。
4. **iface 决策泛化**:`decide_ipv6_route` 加**网关**分支(off-link + 有 v6 网关 → `Gateway{next_hop}`;镜像 `decide_ipv4_route`)。网关来源 = best_ipv6_route 的 decision.next_hop(与 v4 一致:FIB 与 iface 各算一次 next_hop)。
5. 网关 MAC 由 **V2 NDP** 动态解析(已就绪)。

### 4.2 V3b 文件计划

| 文件 | 改动 |
|---|---|
| `protocol/tcp.rs` / `protocol/udp.rs` | `emit_ipv6_packet`(mirror emit_ipv4_packet) |
| `execution/step_device_tx.rs` | TCP/UDP/raw 发包按 family 选 v4/v6 emit |
| `execution/step_send.rs` / `step_connect.rs` | v6 dst 走 `best_ipv6_route` |
| `protocol/ether/mod.rs` | `decide_ipv6_route` 加 `Gateway` 分支 + `Ipv6RouteDecision::Gateway` |
| `tests/external_connect_tests.rs` | 外部 v6 TCP/UDP 收发(mirror v4 external_connect) |

---

## 5. 需你拍板的决策(附推荐)

| # | 决策 | 推荐 | 理由 |
|---|---|---|---|
| D1 | v6 路由表:独立 `routes6` vs enum 化 v4 结构 | **独立表** | 镜像 ARP→NDP 独立表模式;零 v4 波及(enum 化会牵动 /proc/net/route、rtnetlink v4、全消费者) |
| D2 | 本轮范围:V3a-only vs V3a+V3b 一起 | **V3a-only 先做** | V3a 自足、镜像 v4、低风险、解锁 `ip -6 route`;V3b 触热数据路径(emit/device_tx),单独审查更稳(§8) |
| D3 | `/proc/net/ipv6_route` 格式 | **Linux 精确格式** | judge/命令按真实格式解析(dst/src/nexthop 各 32hex + prefixlen + flags) |
| D4 | (V3b)`decide_ipv6_route` 网关来源 | **同 v4 双算** | FIB(best_ipv6_route)与 iface(decide)各算 next_hop,和 v4 完全对称 |

**推荐组合**:先按 **V3a-only** 实现(独立 v6 表 + rtnetlink v6 route + /proc/net/ipv6_route),验完再单独审 V3b。若你要一次到底(V3a+V3b)也可,但 V3b 的 emit/device_tx 改动会碰 v4 热路径,回归面更大。

---

## 6. 验证门

| 门 | V3a | V3b |
|---|---|---|
| 编译 | `cargo xtask build --target rv64-qemu` 干净 | 同 |
| 回归 | `cargo test -p tx-subsystems` 集合差=基线(**v4 路由/proc/rtnetlink 不回归**;隔离复跑新测试) | 同 + **v4 external TCP/UDP + v6 loopback 不回归** |
| 单测 | rtnetlink v6 route round-trip + /proc/net/ipv6_route 渲染 | 外部 v6 TCP/UDP 收发(external_connect v6) |
| 功能(需真机) | QEMU `ip -6 route add/del/show`、`cat /proc/net/ipv6_route` | QEMU 外部 v6 iperf/netperf |

**硬门槛**:V3a 全程**不碰数据面**,所以 v4/v6 现有流量零改变——回归面仅限管理面/投影。

---

## 7. 风险与回退

- **V3a 风险低**:纯加法(新表 + 新 rtnetlink 分支 + 新 proc 文本),不改 v4 路径。回退 = 移除 v6 分支即回到今天。
- **V3b 风险中**:`step_device_tx` / `emit_*` 是 v4 热路径,加 family 分流须保证 v4 分支字节级不变(`emit_ipv4_packet` 调用点不动,只在 family==Inet6 时走新分支)。回退 = family 分流退回恒 v4。
- **rtnetlink family 分流坑**:`handle_newroute` 现无条件 `add_ipv4_route`;分流前须确认 `parse_route_config` 对 AF_INET6 的现有半成品分支(:986/1020)不会把 v6 报文误当 v4。

---

## 8. 为什么推荐 V3a 先行

1. **自足**:`ip -6 route` / `/proc/net/ipv6_route` 不依赖数据面即可工作、可验(命令回显 + 单测)。
2. **镜像 v4**:结构/rtnetlink/proc 全有 v4 原件,1:1 照抄,认知负担小。
3. **低风险**:不碰 `emit_*` / `step_device_tx` 热路径,回归面仅管理面。
4. **解锁 LTP v6 路由管理靶**(`ip -6 route` 族)与命令类计分,回报直接。
5. V3b(外部 v6 真流量)价值取决于是否有 v6 iperf/netperf 靶——可在 V3a 落地后按靶单独立项审查。

---

## 附:V3a 开工顺序(实现时)

1. `NetNamespaceRoute6{Entry,Info,Config,Selector,Decision}` + `routes6` 字段 + 构造初始化(骨架,编译过)。
2. `route6_snapshot`(连接路由合成 + 显式 + 最长前缀排序)+ `add_ipv6_route`/`delete_ipv6_route`/`best_ipv6_route`/`route6_decision_for_info`(照抄 v4)。
3. `proc_net_ipv6_route_snapshot_text`(Linux v6 格式)+ procfs 挂载。
4. rtnetlink:newroute/delroute family 分流 + `parse_route6_config` + `render_getroute_dump` v6 + `build_route6_message`。
5. `tests/rtnetlink_tests.rs`:v6 route round-trip + proc 渲染。
6. build + 集合差回归 +(可选)QEMU `ip -6 route`。

每步可编译、可回归;不碰数据面。
