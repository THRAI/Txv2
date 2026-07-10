# IPv6 补全 · V1 实现方案(外部 L3 发包链 + ICMPv6 收包)

<!-- txdoc:07-NET-IPV6-V1-PLAN-V1 -->

> **状态:待审查(未实现)。** B 路第一阶段。总纲 [`IPV6_STATUS_v1.md`](IPV6_STATUS_v1.md)。本文只讲"要改什么、照哪段 v4 mirror、seam 在哪、哪些点需你拍板",证据带 `file:line`。审完再动手。

---

## 0. V1 目标与边界

**目标**:让 **IPv6 包能出网卡**(外部 TX 成形)+ **ICMPv6 能收进来**(RX demux 不再丢)。这是解锁"外部 v6 完全不通"的总闸。

**V1 做**:
1. `dispatch_ipv6_at` —— v6 版的 L3 出口(解析→路由→解 MAC→发帧)。
2. `build_ipv6_ethernet_frame` —— v6 版 L2 帧封装。
3. `decide_ipv6_route`(**最小版**:multicast / on-link / 网关 / unreachable)。
4. `resolve_ndisc`(**静态版**:查 `ndisc_table` + v6 组播 MAC 推导)。
5. ICMPv6 RX demux 臂(`smoltcp_demux` 不再把 Icmpv6 当 Unsupported)。
6. TX 入口 **v4/v6 分叉**(peek IP version)。

**V1 不做(明确延后)**:
- 动态 NDP(NS/NA/RS/RA + RX 学习)→ **V2**(V1 只静态查表,查不到就 Failed)。
- 完整路由/FIB + `/proc/net/ipv6_route` → **V3**(V1 只 on-link + 单网关)。
- v6 分片/重组 → **V4**(V1 超 MTU 直接 EMSGSIZE —— 符合 v6 语义:路由器不分片)。

---

## 1. IPv4 蓝本(照这条链 mirror)

现有 IPv4 出口链(全在 `protocol/ether/mod.rs`),干净、逐层清晰:

```
TX sink.transmit (:655) ──► dispatch_ip_at (:308)
                              │  ① len 检查
                              │  ② Ipv4Packet::new_checked(:316)
                              │  ③ decide_ipv4_route(:325 → :906)  → next_hop
                              │  ④ resolve_or_request(:336)  → dst_mac(ARP)
                              │  ⑤ transmit_ipv4_packet(:564)
                              │        ≤MTU → build_ipv4_ethernet_frame(:664) → transmit_frame(link.rs:213)
                              │        >MTU → transmit_ipv4_fragments
```

关键子件:
- `decide_ipv4_route`(:906):`broadcast → Direct(同网段) → Gateway(common.gateway()) → Unreachable`。
- `resolve_or_request`(ARP,:~330 前):`BROADCAST→bcast mac / lookup_arp_entry 命中→Resolved / 未命中→queue_pending_arp`。
- `build_ipv4_ethernet_frame`(:664):套 `EthernetRepr{ethertype: Ipv4}` + 拷 payload。
- RX 侧:`demux_rx_frame_with_smoltcp`(smoltcp_demux.rs:11)→ `demux_ipv6`(:28),v6 只有 Tcp/Udp 臂,`_ => Unsupported`(:39)。

---

## 2. V1 具体改动(逐件,带 seam + 代码骨架)

### 2.1 TX 入口 v4/v6 分叉 —— seam:`dispatch_ip_at` 顶部 peek version

**问题**:TX sink `transmit`(:655)硬调 `dispatch_ip_at`,后者 `Ipv4Packet::new_checked` 对 v6 包直接 EINVAL(:316-324)。
**改法**:在 `dispatch_ip_at` 顶部 peek 首字节高 4 位(IP version),分叉:

```rust
pub fn dispatch_ip_at(&self, packet: &[u8], now, guard) -> PacketTxResult {
    match packet.first().map(|b| b >> 4) {
        Some(6) => self.dispatch_ipv6_at(packet, now, guard),   // 新增
        Some(4) => self.dispatch_ipv4_at(packet, now, guard),   // 现有 body 抽成 _ipv4_at
        _ => PacketTxResult::Failed { errno: Errno::EINVAL },
    }
}
```
> 备选:分叉放在 `EtherPacketTxSink::transmit`(:655),但 `dispatch_ip_at` 顶部更集中、少改调用点。**推荐前者。**

### 2.2 `dispatch_ipv6_at` —— mirror `dispatch_ip_at`

```rust
fn dispatch_ipv6_at(&self, packet: &[u8], now: Instant, guard: &Guard<'_>) -> PacketTxResult {
    let ipv6 = Ipv6Packet::new_checked(packet)?; // Err → EINVAL(同 v4)
    let route = decide_ipv6_route(/*v6 config*/, from_smoltcp_ipv6(ipv6.dst_addr()));
    let next_hop = route.next_hop()?;            // None → EADDRNOTAVAIL(同 v4)
    let dst_mac = match self.resolve_ndisc(next_hop, now) {
        NdiscResolution::Resolved { mac } => mac,
        NdiscResolution::Failed { errno } => return PacketTxResult::Failed { errno },
        // V1 无 Pending(动态 NDP 是 V2)
    };
    self.transmit_ipv6_packet(dst_mac, packet, guard)
}
```

### 2.3 `decide_ipv6_route`(最小版)—— ⚠️ **最大设计点**

mirror `decide_ipv4_route`,但**卡在数据来源**:`IfaceCommon`(loopback.rs:12)**只有 v4 字段**(`ipv4_addr/netmask/gateway: Option<Ipv4Address>`),**没有 v6 地址/前缀/网关**。而 iface 的 v6 地址其实存在别处(namespace link snapshot 的 `ipv6_addr`/`ipv6_prefix_len`,控制面已有)。

**要你拍板的选项(A/B)**:
- **A. 给 `IfaceCommon` 加 v6 字段**(`ipv6_addr: Option<Ipv6Address>`, `ipv6_prefix_len: Option<u8>`, `ipv6_gateway: Option<Ipv6Address>`)——最对称、`decide_ipv6_route(common, dst)` 签名和 v4 一致;代价:动 `IfaceCommon` 构造点 + 得从 namespace 把 v6 config 灌进来。
- **B. `decide_ipv6_route` 收显式 v6 config 参数**(不动 IfaceCommon)——`decide_ipv6_route(v6cfg, dst)`,由 `dispatch_ipv6_at` 从 iface/namespace 现取;代价:签名不对称,但改动面小。

**注意:v6 网关字段目前可能根本没有**(控制面调研只见 v6 地址/前缀,未见 v6 gateway)——需确认;若无,V1 先支持 **on-link only**(同前缀 Direct;跨网段 Unreachable),v6 网关留到 V3 随 FIB 一起。骨架:
```rust
fn decide_ipv6_route(cfg, dst: Ipv6Address) -> Ipv6RouteDecision {
    if dst.is_multicast()            { Multicast { dst } }        // → 组播 MAC
    else if same_ipv6_prefix(cfg, dst) { Direct { next_hop: dst } } // on-link
    else if let Some(gw) = cfg.gateway6 { Gateway { next_hop: gw } } // 若有 v6 网关
    else { Unreachable { dst } }
}
```

### 2.4 `resolve_ndisc`(静态版,V1)—— mirror `resolve_or_request`

```rust
fn resolve_ndisc(&self, next_hop: Ipv6Address, now: Instant) -> NdiscResolution {
    if next_hop.is_multicast() {
        return NdiscResolution::Resolved { mac: multicast_mac_for(next_hop) }; // 33:33:xx:xx:xx:xx
    }
    match self.ndisc_table.lock().get(&next_hop) {           // 静态表(:129)
        Some(entry) => NdiscResolution::Resolved { mac: entry.mac },
        None => NdiscResolution::Failed { errno: Errno::EHOSTUNREACH }, // V2 改成发 NS + Pending
    }
}
```
> `NdiscEntry`(:52)已带 mac;组播 MAC 推导 = `33:33` + v6 地址后 4 字节(RFC 2464)。

### 2.5 `transmit_ipv6_packet` + `build_ipv6_ethernet_frame`

```rust
fn transmit_ipv6_packet(&self, dst_mac, packet, guard) -> PacketTxResult {
    if packet.len() <= usize::from(self.common.mtu()) {
        let frame = build_ipv6_ethernet_frame(self.ether_addr, dst_mac, packet);
        return self.transmit_frame(&frame, guard);
    }
    PacketTxResult::Failed { errno: Errno::EMSGSIZE }  // V1 不分片(v6 语义:源端才分片,交 V4)
}

fn build_ipv6_ethernet_frame(src, dst, ipv6_packet: &[u8]) -> Vec<u8> {
    // 与 build_ipv4_ethernet_frame(:664)逐字一样,只 ethertype 改 Ipv6
    EthernetRepr { src_addr, dst_addr, ethertype: EthernetProtocol::Ipv6 } ...
}
```

### 2.6 ICMPv6 RX demux 臂

`smoltcp_demux.rs::demux_ipv6`(:28),把 `_ => Unsupported`(:39)替换/补成 ICMPv6 臂:
```rust
IpProtocol::Icmpv6 => /* parse ICMPv6 → PacketDispatch::Icmp6(...) */,
```
接到**已存在**的 raw ICMPv6 投递路径(`step_send.rs:712 deliver_raw_ipv6_packet_to_table`,已按 family/proto/icmp6_filter 过滤)。**需确认 `PacketDispatch` 有没有 v6 ICMP 变体**(v4 是 `PacketDispatch::Icmp`);若无,加 `Icmp6` 变体 + 下游 dispatch 一臂。

---

## 3. 待你拍板的开放问题

1. **§2.3 的 A/B**:v6 config 放进 `IfaceCommon`(A,对称但动构造)还是显式传参(B,不动 IfaceCommon)?**我倾向 A**(长期对称,V2/V3 都要复用 v6 config)。
2. **v6 网关**:当前有没有 iface 级 v6 网关字段?若无,V1 先 **on-link only**、v6 网关随 V3 FIB?(**我倾向是**——V1 保持最小。)
3. **ICMPv6 RX 落点**:只接到 raw-icmp6 socket(够 ping6 外部)即可,还是 V1 也要处理收到的 NS/NA(那其实是 V2 的活)?**建议 V1 只 raw-icmp6 收包,NS/NA 处理留 V2。**
4. **分叉位置**(§2.1):`dispatch_ip_at` 顶部 peek(推荐)vs sink 层?

---

## 4. 验证门(V1)

- **硬门槛:v6 loopback 不回归**(`step_udp/tcp_loopback` v6 现在唯一跑通的数据路径)+ `cargo test -p tx-subsystems` 集合差=基线 + rv64/la64 双架构编译。
- **正向**:构造一个外部 v6 目的的 UDP/raw send → 抓 `netdev.ops` 发出的帧,确认是 `ethertype=0x86DD` 的合法 v6 帧、dst MAC = 静态 ndisc 配置值。
- **ICMPv6 RX**:注入一个 ICMPv6 echo-reply v6 帧 → 确认 demux 不再 Unsupported、投到 raw-icmp6 socket。
- **回归面**:`cargo test` 覆盖 demux/loopback/dispatch 的现有 v4+v6 测试全绿。

---

## 5. 改动清单预估

| 文件 | 改动 | ~LOC |
|---|---|---|
| `protocol/ether/mod.rs` | dispatch 分叉 + dispatch_ipv6_at + transmit_ipv6_packet + build_ipv6_ethernet_frame + decide_ipv6_route + resolve_ndisc + (可能)IfaceCommon v6 字段 | ~120-160 |
| `protocol/loopback.rs`(IfaceCommon) | (选 A 时)加 v6 字段 + 构造 | ~20-30 |
| `packet/smoltcp_demux.rs` | ICMPv6 demux 臂 | ~15-25 |
| `PacketDispatch`(定义处) | (若需)Icmp6 变体 + 下游一臂 | ~10-20 |
| 测试 | v6 dispatch/frame/demux 单测 | ~60-100 |

**净新增 ~180-250 LOC**(含测试),是"照 v4 复制一份 v6 转发面"的第一块——正是 B 路的固有代价。
