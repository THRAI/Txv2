<!-- txdoc:07-NET-IPV6-V2-PLAN-V1 -->

# IPv6 V2 实现计划:动态 NDP(邻居发现)

> 前置:[[IPV6_STATUS_v1]] §5 表 V2 行、[[IPV6_V1_PLAN_v1]]、已提交 V1(commit a90988d0)。
> 本文是 **V2 实现前的调研稿**,待审查后再动手(遵循每阶段"先调研→出文档→审查→实现"流程)。

---

## 1. 目标与范围

**V2 = 把 v6 下一跳的 MAC 解析从"静态 `ndisc_table`"升级为"动态 NDP 收发学习"**,让外部 v6 邻居无需预置即可解析——正是 v4 ARP 的 v6 对应物。

**解锁**:`ping6 外部网关`(无需手工 `ip -6 neigh add`)、外部 v6 TCP/UDP 的下一跳自动解析。

**交付**(镜像 v4 ARP 动态机):
1. **RX 学习**:收到 NS/NA 时,把对端 `IPv6 地址→MAC` 学进 `ndisc_table`。
2. **RX 应答**:收到"目标是本机 v6 地址"的 NS 时,回 NA(让外部能解析本机)。
3. **TX 探测**:`resolve_ndisc` 未命中时,发 NS(送 solicited-node 组播),排入 pending,按退避重试;NA 到达即解析。

**非目标(推给 V3/V4/后续)**:
- **路由器发现 RS/RA**(SLAAC/前缀/默认路由)——需要 FIB/地址自动配置,归 V3 路由。
- **DAD 重复地址检测**——本机只有静态配置地址,暂不做。
- **NUD 完整状态机**(INCOMPLETE/REACHABLE/STALE/PROBE/DELAY)——只做 ARP 同级的"缓存+pending+退避",不做完整 RFC 4861 NUD。
- **v6 分片**、UDP v6 src hint —— V4。

范围严格对齐 v4 ARP 已有的能力面,不多不少。

---

## 2. 背景:NDP 与 ARP 的差异(实现要点)

NDP(RFC 4861)是"ARP for IPv6",但**跑在 ICMPv6 之上**而非独立 EtherType,有 4 个实现差异:

| 维度 | v4 ARP | v6 NDP |
|---|---|---|
| 承载 | 独立 EtherType `0x0806`,独立 `ArpPacket` | **ICMPv6**(next_header=58),EtherType 仍是 `0x86dd`(和普通 v6 报文同) |
| 请求 | ARP Request → 广播 MAC `ff:ff:ff:ff:ff:ff` | **NS**(type 135)→ **solicited-node 组播** `ff02::1:ffXX:XXXX`,MAC `33:33:ff:XX:XX:XX` |
| 应答 | ARP Reply → 单播 | **NA**(type 136)→ 单播,带 flags(S=solicited/O=override) |
| 地址映射 | request/reply 都带 sender IP+MAC | NS 带 **source lladdr option**(解算方 IP→MAC);NA 带 **target lladdr option**(被解析方 IP→MAC) |
| 校验和 | 无 | ICMPv6 校验和覆盖**伪首部**(src/dst v6 地址 + 长度 + next-header) |

**关键**:因为 NS/NA 是 ICMPv6,它们在 RX 路上**和 echo 回包走同一条 `demux_ipv6` → `PacketDispatch::Icmp6`**(V1b 建的路),不像 ARP 有独立 Ethernet 臂。这决定了 RX 拦截点的设计(见 §5.2)。

**solicited-node 组播**:`ff02::1:ff00:0/104`,低 24 位取目标地址低 24 位。对应 MAC = `33:33` + 该组播地址低 4 字节 = `33:33:ff:XX:XX:XX`。**V1a 的 `multicast_mac_for`(33:33+低4字节)对 solicited-node 地址天然正确**,无需新函数。

---

## 3. 要镜像的 v4 ARP 蓝图(`ether/link.rs` + `mod.rs`)

V2 的每个部件都有 v4 原件,逐一对应:

| v4 ARP 部件 | 位置 | V2 NDP 对应 |
|---|---|---|
| `arp_table: BTreeMap<Ipv4Address, ArpEntry{mac,expires_at}>` | mod.rs:128 | `ndisc_table`(**已存在**,V1a 建;`NdiscEntry{mac,expires_at}` 已带 TTL) |
| `pending_arp: BTreeMap<Ipv4Address, ArpPendingEntry{ip,attempts,next_probe_at,last_error}>` | mod.rs | **新增** `pending_ndisc: BTreeMap<Ipv6Address, NdiscPendingEntry>` |
| `process_arp(payload,now,guard)`:parse→learn→(request 命中本机则 reply) | link.rs:4 | **新增** `process_ndisc`:parse ICMPv6/Ndisc→learn→(NS 命中本机则回 NA) |
| `learn_arp(ip,mac,now)`:插表+清 pending+stat | link.rs:75 | **新增** `learn_ndisc(ip,mac,now)` |
| `lookup_arp_entry(ip,now)`:TTL 检查缓存 | link.rs:87 | **新增** `lookup_ndisc_entry`(或内联进 `resolve_ndisc`) |
| `resolve_or_request(next_hop,now)`:命中→Resolved / miss→queue_pending | link.rs:99 | **升级** `resolve_ndisc`(现只静态查+miss 直接 Failed) |
| `queue_pending_arp` / `ready_pending_arp` / `pending_entry_for_probe` / `mark_arp_probe_sent` | link.rs:115-177 | **新增** 4 个 `*_ndisc` 对应 |
| `build_arp_request` / `build_arp_reply` | link.rs:187-211 | **新增** `build_neighbor_solicit` / `build_neighbor_advert` |
| `flush_pending_arp_at(now,budget,guard)`:探测驱动循环 | mod.rs:363 | **新增** `flush_pending_ndisc_at`(或并进同一 step) |
| `step_flush_pending_arp`(reactor step) | execution/step_flush_pending_arp.rs:21 | **扩展**该 step 同时驱动 v4+v6 探测(见 §5.4) |
| 常量 `ARP_CACHE_TTL=300s` / `RETRY_LIMIT=3` / `RETRY_DELAY=1s` | mod.rs:26-28 | **复用**(或加 `NDISC_` 同值别名) |

TX 侧的 pending 语义也照抄:`transmit_ipv4_packet` 在 `ArpResolution::Pending` 时**不缓存报文**,直接返回 `PacketTxResult::PendingResolution{next_hop}`,由上层重传(mod.rs:351)。V2 同样**不缓存 v6 报文**——首包触发 NS 后被丢/pending,重传包解析成功(和真实 ARP/NDP 首包行为一致)。

---

## 4. 决定性利好:smoltcp-asterinas 暴露完整 NDP 编解码

`external/smoltcp-asterinas`(仓内 path 依赖)`src/wire/` 已导出(`wire/mod.rs:254-265`):
- `Icmpv6Repr`(icmpv6.rs:574),含 **`Ndisc(NdiscRepr)` 变体**(:607)
- `NdiscRepr`(ndisc.rs:194),含 `NeighborSolicit{target_addr, lladdr}`(:208)、`NeighborAdvert{flags, target_addr, lladdr}`(:212),**parse(:229)+emit(:346) 都有**
- `NdiscNeighborFlags`(S/O/R)、`NdiscOption`/`NdiscOptionRepr`(lladdr option)

**含义**:V2 **不手搓 NS/NA wire**,完全复用 smoltcp——与 V1a/ARP 复用 `ArpRepr`、V1b 复用 `Ipv6Packet` 同一路数。构建 NS = `Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit{...}).emit()`(emit 需 src/dst 算 ICMPv6 伪首部校验和)→ 得 ICMPv6 报文 → 复用 **V1a 的 `build_ipv6_ethernet_frame`** 套 IPv6+Ethernet 头。解析同理:`Icmpv6Packet::new_checked` + `Icmpv6Repr::parse(&pkt, &src, &dst, &caps)`。

这把 V2 的实现量压到"照抄 ARP 状态机 + 调 smoltcp 编解码",无新协议栈。

---

## 5. 设计

### 5.1 数据结构(EtherIface 内,mod.rs)

```rust
// 复用现有 NdiscEntry{mac, expires_at}(V1a 已建,已带 TTL)。
// 静态 add(现 mod.rs:442)继续用——写同一张 ndisc_table,expires_at 给远期/续期即可。

// 新增 pending 表(镜像 ArpPendingEntry):
pub struct NdiscPendingEntry {
    pub addr: Ipv6Address,
    pub attempts: u8,
    pub next_probe_at: Instant,
    pub last_error: Option<Errno>,
}
// EtherIface 加字段:
pending_ndisc: SpinMutex<BTreeMap<Ipv6Address, NdiscPendingEntry>>,
// (可选)ndisc_stats: 对齐 arp_stats 的计数器,喂 /proc 投影。
```

`NdiscResolution` enum(现 `Resolved/Failed`)**加 `Pending{next_hop: Ipv6Address}`**,与 `ArpResolution` 三态对齐。

### 5.2 RX:拦截 NS/NA(推荐镜像 `maybe_reply_icmpv4`)

现 v4 RX 臂(mod.rs:295-298)用 **side-effect peek** 处理 ICMPv4 echo:
```rust
let dispatch = demux_rx_frame_with_smoltcp(&RxFrame::new(frame));
self.maybe_reply_icmpv4(&dispatch, now, guard);   // 副作用:回 echo reply,不改 dispatch
dispatch
```

**V2 镜像它**——把现 v6 臂(mod.rs:304)`EthernetProtocol::Ipv6 => demux_rx_frame_with_smoltcp(&frame)` 改成:
```rust
EthernetProtocol::Ipv6 => {
    let dispatch = demux_rx_frame_with_smoltcp(&frame);
    self.maybe_process_ndisc(&dispatch, now, guard);   // 新增:NS/NA 学习+回 NA
    dispatch
}
```
`maybe_process_ndisc(&PacketDispatch, now, guard)`:
- 只认 `PacketDispatch::Icmp6(pkt)`,把 `pkt.payload` 当 ICMPv6 报文 `Icmpv6Repr::parse`(用 `pkt.src`/`pkt.dst` 算校验和)。
- 命中 `Ndisc(NeighborSolicit{target_addr, lladdr})`:若有 source lladdr 且 `pkt.src` 单播 → `learn_ndisc(pkt.src, lladdr, now)`;若 `target_addr == 本机 v6 addr` → `build_neighbor_advert` 回单播 NA。
- 命中 `Ndisc(NeighborAdvert{target_addr, lladdr, ..})`:若有 target lladdr → `learn_ndisc(target_addr, lladdr, now)`。
- 其它(echo/RS/RA)→ 忽略(dispatch 照常送 raw socket)。

**为什么用 side-effect peek 而非 Ethernet 臂分流**(如 ARP):NS/NA 是 ICMPv6,和 echo 回包共用 `demux_ipv6`;peek 方式**零改 demux、echo raw 路不动、最小侵入**,且 NS/NA 顺带进 raw socket 无害(Linux 下 raw ICMPv6 本就收得到 NS/NA)。这是 §7 决策 D1。

### 5.3 TX:`resolve_ndisc` 静态→动态

现 `resolve_ndisc`(mod.rs)忽略 `now`、miss 直接 `Failed`。**升级为**(镜像 `resolve_or_request`):
```rust
fn resolve_ndisc(&self, next_hop, now) -> NdiscResolution {
    if next_hop.is_multicast() { return Resolved{ multicast_mac_for(next_hop) }; }  // 不变
    if let Some(entry) = self.lookup_ndisc_entry(next_hop, now) {                    // 新:TTL 检查
        return Resolved{ entry.mac };
    }
    self.queue_pending_ndisc(next_hop, now)   // 新:miss→排 pending,返回 Pending/Failed(超限)
}
```
`transmit_ipv6_packet`(V1a)现在只认 `Resolved`——**加 `Pending → PacketTxResult::PendingResolution{next_hop}`**(镜像 mod.rs:351 的 v4 处理),让上层重传。

### 5.4 探测驱动:`flush_pending_ndisc_at` + step 扩展

新增 `flush_pending_ndisc_at(now, budget, guard)`,循环体照抄 `flush_pending_arp_at`(mod.rs:363):`ready_pending_ndisc` → `pending_entry_for_probe`(超 `RETRY_LIMIT` 标 `EADDRNOTAVAIL`)→ `build_neighbor_solicit(target)` → `transmit_frame` → `mark_ndisc_probe_sent`。

**驱动挂载**(决策 D5):**扩展现有 `step_flush_pending_arp`**(execution/step_flush_pending_arp.rs:21)让它一个 reactor tick 同时驱动 v4 ARP + v6 NDP 探测,而非新建独立 step。避免多一个 reactor 任务。

### 5.5 NS/NA 构建(复用 smoltcp + V1a 帧构建)

```rust
fn build_neighbor_solicit(&self, target: Ipv6Address) -> Vec<u8> {
    let sn_mcast = solicited_node_multicast(target);          // ff02::1:ff + 低24位
    let repr = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
        target_addr: target.into(),
        lladdr: Some(self.ether_addr.into()),                 // source lladdr option
    });
    // emit → ICMPv6(校验和 src=本机 v6 addr, dst=sn_mcast)→ IPv6 头 → build_ipv6_ethernet_frame(dst_mac=33:33:ff:..)
}
fn build_neighbor_advert(&self, to_ip, to_mac, target) -> Vec<u8> {
    // flags = SOLICITED|OVERRIDE, target lladdr = 本机 MAC, 单播回 to_ip/to_mac
}
```
`solicited_node_multicast(target)` 是唯一新增地址助手(§2)。framing 全走 V1a 的 `build_ipv6_ethernet_frame`。

---

## 6. 逐文件改动计划

| 文件 | 改动 | 量级 |
|---|---|---|
| `protocol/ether/mod.rs` | `NdiscResolution` 加 `Pending`;`resolve_ndisc` 动态化;`transmit_ipv6_packet` 认 `Pending`;`pending_ndisc` 字段 + `NdiscPendingEntry`;`flush_pending_ndisc_at`;`solicited_node_multicast`;`maybe_process_ndisc` 调度 | 中(~120) |
| `protocol/ether/link.rs`(或新 `ndisc.rs` 兄弟文件) | `process_ndisc`/`learn_ndisc`/`lookup_ndisc_entry`/`queue_pending_ndisc`/`ready_pending_ndisc`/`pending_entry_for_ndisc_probe`/`mark_ndisc_probe_sent`/`build_neighbor_solicit`/`build_neighbor_advert` | 中(~180,照抄 ARP) |
| `execution/step_flush_pending_arp.rs` | 追加 `iface.flush_pending_ndisc_at(...)` 调用(或抽 `step_flush_pending_neighbors`) | 小(~10) |
| `structure/types.rs` | (若需)`Ipv6Address` 与 smoltcp `Ipv6Address` 互转已在 V1a;solicited-node 计算可放这或 mod.rs | 小 |
| `tests/…` | 新增 `ether_iface_ndisc_tests`:NS miss 发解算、NA 学习、NS-for-us 回 NA、retry-limit→Failed(镜像 `ether_iface_arp_tests`) | 中 |

**决策**:`process_ndisc` 一族放 `link.rs` 还是新建 `protocol/ether/ndisc.rs`?link.rs 已是"L2 邻居解析"归属地(ARP 在此),NDP 同类——**建议放 link.rs**(改名注释为"neighbor resolution: ARP + NDP"),保持单一邻居层。这是决策 D6。

---

## 7. 需你拍板的决策(附我的推荐)

| # | 决策 | 推荐 | 理由 |
|---|---|---|---|
| D1 | RX 拦截方式:side-effect peek(`maybe_process_ndisc`)vs Ethernet 臂分流(如 ARP) | **peek** | 镜像现有 `maybe_reply_icmpv4`;零改 demux;echo raw 路不动;NS/NA 顺带进 raw 无害 |
| D2 | 静态 `ndisc_table` add 是否保留 | **保留** | 动态学习写同一表;静态 add 作预置项(TTL 远期);无缝共存 |
| D3 | 是否回 NA(应答 NS-for-us) | **回** | 镜像 ARP request→reply;外部要能解析本机才能双向 ping6 |
| D4 | Pending 时是否缓存待发 v6 报文 | **不缓存** | 照抄 v4(返回 `PendingResolution`,上层重传);最简、和 ARP 一致 |
| D5 | 探测驱动:扩展 `step_flush_pending_arp` vs 新 step | **扩展现有** | 一个 reactor tick 驱动 v4+v6,少一个任务 |
| D6 | `process_ndisc` 一族放 link.rs vs 新 ndisc.rs | **link.rs** | 与 ARP 同属"邻居解析层",单一归属 |
| D7 | NDP 常量:复用 `ARP_*` vs 新 `NDISC_*` | **新 `NDISC_*` 同值别名** | 语义清晰、日后可独立调(默认值同 ARP:TTL 300s/retry 3/delay 1s) |

若无异议,我按以上推荐实现(等你一句"按推荐做")。

---

## 8. 验证门(实现后)

| 门 | 判据 |
|---|---|
| 编译 | `cargo xtask build --target rv64-qemu` 干净 |
| 回归 | `cargo test -p tx-subsystems --lib` 集合差 = 基线 313,**v6 loopback + v4 ARP 均不回归** |
| 单测 | 新 `ether_iface_ndisc_tests`:NS 发送/NA 学习/NS-for-us 回 NA/retry-limit→Failed(镜像 arp 测试全绿) |
| 功能(可选,需真机) | QEMU 外部 `ping6 <网关>`——首包触发 NS、NA 学习、后续包通(§5 表 V2 行判据) |

**硬门槛**:v4 ARP 一个用例都不能动(共用 `flush` step、共用 `link.rs`,改动必须与 v4 正交)。

---

## 9. 风险与回退

- **风险 1**:ICMPv6 校验和伪首部算错 → NA/NS 被对端丢弃。缓解:smoltcp `emit`/`parse` 全权负责校验和,传对 src/dst 即可;单测断言 round-trip。
- **风险 2**:`maybe_process_ndisc` 双解析(demux 已解一次)。可接受(纳秒级);若在意可后续把类型 peek 结果透传。
- **风险 3**:pending 表与 arp pending 竞争同一 `flush` budget。缓解:budget 分别计数或共享(v4/v6 通常不同时大量 miss)。
- **回退**:V2 全部加法(新表/新函数/新 peek 调用 + `resolve_ndisc` 一处升级 + `transmit_ipv6_packet` 加一分支)。回退 = 把 `resolve_ndisc` miss 改回 `Failed`、摘掉 `maybe_process_ndisc` 调用即回到 V1 静态行为。

---

## 附:开工顺序(实现时)

1. `NdiscResolution::Pending` + `NdiscPendingEntry` + `pending_ndisc` 字段 + `NDISC_*` 常量(骨架,编译过)。
2. `link.rs`:`learn_ndisc`/`lookup_ndisc_entry`/`queue_pending_ndisc`/`ready_pending_ndisc`/`pending_entry_for_ndisc_probe`/`mark_ndisc_probe_sent`(纯状态机,照抄 ARP)。
3. `build_neighbor_solicit`/`build_neighbor_advert` + `solicited_node_multicast`(复用 smoltcp emit + V1a framing)。
4. `resolve_ndisc` 动态化 + `transmit_ipv6_packet` 认 `Pending`。
5. `process_ndisc` + `maybe_process_ndisc` + RX 臂挂载。
6. `flush_pending_ndisc_at` + `step_flush_pending_arp` 扩展。
7. `ether_iface_ndisc_tests`。
8. build + 集合差回归 + (可选)QEMU ping6。

每步可编译、可回归;3-4 可独立验(静态仍工作),5-6 打通动态。
