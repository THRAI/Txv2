<!-- txdoc:07-NET-IPV6-V3B-PLAN-V1 -->

# IPv6 V3b 实现计划:v6 数据面(外部 off-link TCP/UDP)

> 前置:[[IPV6_STATUS_v1]]、[[IPV6_V3_PLAN_v1]](V3 拆 a/b 的由来)、已提交 V1(a90988d0)+ V2(ccae95c8)+ V3a(6ee95e0b)。
> 本文是 **V3b 实现前的调研稿**,待审查后再动手。

---

## 1. 调研结论:范围远小于 [[IPV6_V3_PLAN_v1]] §4 的估计

V3 计划里 §4 把 V3b 估为"`emit_ipv6_packet` + step_device_tx v6 分支 + step_send/connect v6 选路 + 网关"——**深挖后发现前三项其实早已就绪**(代码库既有 + V1a/V2/V3a 铺垫)。**唯一真缺口 = `decide_ipv6_route` 的 off-link 网关分支**。

### 1.1 已就绪(V3b 不用做)

| 部件 | 现状(实测代码) |
|---|---|
| **TCP 成包** | `SmoltcpTcpSegment::emit_ipv4_packet`(tcp.rs:525)用存储的 `self.ip_repr`,而 `ip_repr` 可为 `IpRepr::Ipv6`(from_reprs, tcp.rs:565)→ **名字骗人,实为双族**,v6 连接自然发 v6 段 |
| **UDP 成包** | `UdpTxDatagram::emit_ipv4_packet`(udp.rs:494)首行 `if self.dst.family == Inet6 { return self.emit_v6(src) }`(udp.rs:498)→ **已双族**,`emit_v6` 建完整 IPv6/UDP 包 |
| **UDP 源端点** | step_device_tx `local = udp_local_endpoint(protocol_snapshot)`(:299)= socket 自身绑定端点 → **v6 socket 得 v6 local** |
| **connect 源地址选择** | step_connect(778-796)**已双族**:v4 dst→`best_ipv4_route.preferred_src`;v6 dst→取 link 的 `ipv6_addr` |
| **on-link v6 路由** | `decide_ipv6_route`(V1a):multicast→Multicast、**同前缀→Direct**→`resolve_ndisc`(V2 NDP)→wire |

**推论**:**on-link 外部 v6 TCP/UDP 很可能今天就能通**(所有部件齐备),只是**没有测试**验证过。

### 1.2 真缺口(V3b 要做)

| 缺口 | 说明 |
|---|---|
| **`decide_ipv6_route` off-link 网关** | 现在 off-link(非同前缀、非组播)→ `Unreachable`。需加 `Gateway{next_hop=v6网关}` 分支(mirror `decide_ipv4_route`) |
| **IfaceCommon 无 v6 网关** | `decide_ipv6_route(self.common, dst)` 是纯函数只吃 `IfaceCommon`;v4 网关在 `IfaceCommon.gateway`,**v6 无对应字段**(loopback.rs:17-18 注释明写"gateway lands with the V3 FIB") |
| **v6 网关灌入** | v4 网关经 `gateway_for_device`(从 `routes` 默认路由 0.0.0.0/0 解析)→ `ensure_ether_iface_for_link` → `IfaceCommon::with_gateway`。v6 需 `gateway6_for_device`(从 V3a 的 `routes6` 默认路由 ::/0 解析)→ 灌进 IfaceCommon |
| **无外部 v6 测试** | `external_connect_tests.rs` 零 v6 用例;需补 on-link + off-link |

---

## 2. 要 mirror 的 v4 蓝图

- **`IfaceCommon`**(loopback.rs:12):`ipv4_addr/netmask/gateway/mtu/ipv6_addr/ipv6_prefix_len`。`with_gateway(addr,netmask,gateway,mtu)`(:40)带 v4 网关;v6 配置经链式 `with_ipv6(addr,prefix)`(V1a 加,无网关)。
- **`gateway_for_device(name)`**(namespace.rs):扫 `self.routes`,找 `prefix_len==0` 且 oif 匹配的 default route → 其 `gateway`。
- **`ensure_ether_iface_for_link`**(namespace.rs:1936):`gateway = gateway_for_device(link.name)` → 缓存键含 `entry.gateway == gateway` → `IfaceCommon::with_gateway(...).with_ipv6(...)`。`NetNamespaceIfaceRuntime` 有 `gateway: Option<Ipv4Address>`(:223),**无 v6 网关**。
- **`decide_ipv6_route`**(ether/mod.rs:880)+ `Ipv6RouteDecision`(Direct/Multicast/Unreachable,无 Gateway)+ `dispatch_ipv6_at`(:694)已 `decide_ipv6_route → next_hop() → resolve_ndisc`——**网关 next_hop 会自动经 NDP 解析(V2)**,故 dispatch 侧零改。
- **`decide_ipv4_route`**(:1189):`Broadcast / 同子网→Direct / else 有网关→Gateway / else Unreachable`。

---

## 3. 设计(全部加法,不改 v4)

### 3.1 `IfaceCommon` 加 v6 网关(loopback.rs)
- 字段 `ipv6_gateway: Option<Ipv6Address>`,`new`/`with_gateway` 初始化 `None`。
- **链式 setter `with_ipv6_gateway(self, gw) -> Self`**(决策 D1:不改 `with_ipv6` 签名,避免波及 V1a/ndisc 测试的现有 `with_ipv6(addr,prefix)` 调用)。
- getter `ipv6_gateway(self) -> Option<Ipv6Address>`。

### 3.2 `decide_ipv6_route` 加网关分支(ether/mod.rs)
```rust
pub fn decide_ipv6_route(common, dst) -> Ipv6RouteDecision {
    if dst.is_multicast() { Multicast{next_hop:dst} }
    else if same_ipv6_prefix(common, dst) { Direct{next_hop:dst} }
    else if let Some(gw) = common.ipv6_gateway() { Gateway{next_hop:gw} }  // 新
    else { Unreachable{dst} }
}
```
+ `Ipv6RouteDecision::Gateway{next_hop}` 变体,`next_hop()` 返回它(mirror `Ipv4RouteDecision`)。`dispatch_ipv6_at` 无改动(next_hop → `resolve_ndisc(gw)` → NDP 解析网关 MAC → wire)。

### 3.3 v6 网关灌入(namespace.rs)
- `gateway6_for_device(name) -> Option<Ipv6Address>`:扫 `self.routes6`,找 `prefix_len==0`(::/0)且 oif 匹配的 default route → `gateway`(mirror `gateway_for_device`)。
- `NetNamespaceIfaceRuntime` 加 `ipv6_gateway: Option<Ipv6Address>` 字段。
- `ensure_ether_iface_for_link`:`let ipv6_gateway = self.gateway6_for_device(link.name);` + 缓存键加 `entry.ipv6_gateway == ipv6_gateway` + `IfaceCommon…with_ipv6(...).with_ipv6_gateway(ipv6_gateway)` + 两处 runtime 构造赋值。

### 3.4 测试(external_connect_tests.rs)
- **on-link 外部 v6**:v6 socket connect/send 到同前缀 dst,预置 NDP 邻居(或走 NS),断言帧到 device TX 是 v6(验证 1.1 的"已就绪"假设)。
- **off-link 外部 v6**:配 ::/0 via 网关 + 网关 NDP 邻居,connect/send 到异前缀 dst,断言经网关 next_hop 发出(验证 3.2 新分支)。
- mirror 现有 v4 `external_tcp_connect_completes_handshake_from_injected_syn_ack` / `external_udp_sendto_reaches_device_tx`。

---

## 4. 逐文件改动

| 文件 | 改动 | 量级 |
|---|---|---|
| `protocol/loopback.rs` | `IfaceCommon` + `ipv6_gateway` 字段 + `with_ipv6_gateway` + getter | 小(~15) |
| `protocol/ether/mod.rs` | `Ipv6RouteDecision::Gateway` + `next_hop()` + `decide_ipv6_route` 网关分支 | 小(~12) |
| `namespace.rs` | `gateway6_for_device` + `NetNamespaceIfaceRuntime.ipv6_gateway` + `ensure_ether_iface_for_link` 灌入 + 缓存键 | 中(~40) |
| `tests/external_connect_tests.rs` | on-link + off-link 外部 v6 TCP/UDP | 中(~120) |

总计 ~100 生产 LOC + 测试。**零碰 emit/step_device_tx/step_send**(它们已双族)。

---

## 5. 需你拍板的决策(附推荐)

| # | 决策 | 推荐 | 理由 |
|---|---|---|---|
| D1 | IfaceCommon 加 v6 网关:扩 `with_ipv6` 签名 vs 新链式 `with_ipv6_gateway` | **新链式** | 不改 `with_ipv6(addr,prefix)` 现有调用(V1a + ndisc 测试);匹配 V1a 的链式 v6 配置惯例 |
| D2 | v6 网关来源 | **`gateway6_for_device` 从 routes6 ::/0 默认路由解析** | 与 v4 `gateway_for_device` 完全对称;复用 V3a 的 FIB |
| D3 | 本轮是否验证 on-link 已通 | **是,加 on-link 测试** | 1.1 推论需实证;若已通则测试直接绿,新代码只解锁 off-link |

---

## 6. 验证门

| 门 | 判据 |
|---|---|
| 编译 | `cargo xtask build --target rv64-qemu` 干净 |
| 回归 | `cargo test -p tx-subsystems` 集合差=基线(**v4 external TCP/UDP + v6 loopback + v4 路由不回归**;隔离复跑新测试) |
| 单测 | on-link 外部 v6 TCP/UDP 发帧;off-link 经网关 next_hop 发出 |
| 功能(需真机) | QEMU 外部 v6 `ping6`/curl/iperf(若有 v6 靶) |

**硬门槛**:`decide_ipv4_route` / emit / step_device_tx / step_send 字节级不变(V3b 只加 v6 网关分支 + IfaceCommon 字段 + namespace 灌入)。

---

## 7. 风险与回退

- **风险低**:纯加法。`decide_ipv6_route` 加一分支、`IfaceCommon` 加一字段(默认 None → 行为不变)、namespace 加一解析 + 灌入。
- **on-link 若未通**:1.1 是"部件齐备"的推论,实现时先跑 on-link 测试;若有隐藏 v4-only 门(如某处 `family` 判断),就地补——但那属于既有 bug,不属 V3b 新增面。
- **回退**:`decide_ipv6_route` 网关分支去掉即回到 V3a(on-link/unreachable)。

---

## 附:V3b 开工顺序

1. `IfaceCommon` v6 网关字段 + `with_ipv6_gateway` + getter(loopback.rs)。
2. `Ipv6RouteDecision::Gateway` + `next_hop()` + `decide_ipv6_route` 网关分支(ether/mod.rs)。
3. `gateway6_for_device` + `NetNamespaceIfaceRuntime.ipv6_gateway` + `ensure_ether_iface_for_link` 灌入 + 缓存键(namespace.rs)。
4. 先写 on-link 外部 v6 测试(实证 1.1);再写 off-link 经网关测试(验证新分支)。
5. build + 集合差回归 +(可选)QEMU 外部 v6。
