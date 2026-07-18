<!-- txdoc:07-NET-IPV6-V4-PLAN-V1 -->

# IPv6 V4 调研:分片 / 转发 / sysctl —— 价值评估 + 实现草图

> 前置:[[IPV6_STATUS_v1]]、已提交 V1(a90988d0)+ V2(ccae95c8)+ V3a(6ee95e0b)+ V3b(28a05d02)。
> 本文是 **V4 调研稿**。与前几阶段不同,V4 的关键结论是 **ROI 评估**:V4 是架构完整性收尾,**已知 LTP 计分价值近零**,建议**延后**——除非某个具体 v6 靶要求它。

---

## 1. TL;DR

| 结论 | 依据 |
|---|---|
| **UDP v6 源 hint 已完成** | `preferred_ipv6_source_for`(step_send.rs:766)已存在且已接入(:621) |
| **v6 分片(TX+RX)未做,~200+ LOC** | `transmit_ipv6_packet` 超 MTU→EMSGSIZE;`demux_ipv6` 遇 Fragment 头(next=44)→Unsupported 丢 |
| **`ipv6_forwarding` 未做,~100 LOC** | v4 有 `ipv4_forwarding` + 转发路径(bridge/NAT 数据面用);v6 无对等 |
| **v6 forwarding sysctl 未做,~30 LOC** | 已有 `disable_ipv6`/`accept_dad`,无 `/proc/sys/net/ipv6/conf/all/forwarding` |
| **推荐:延后 V4** | **无已知 LTP v6 分片/转发计分靶**;可计分 v6 靶(ipv6_lib/ping6/tracepath/tcpdump)V1-V2 已覆盖;v6 分片实务罕见(PMTUD + 1280 最小 MTU) |

---

## 2. 已完成(V4 不用做)

- **UDP v6 源地址 hint**:`preferred_ipv6_source_for`(step_send.rs:766)从 link 的 v6 地址选源,已接入 UDP 发送路径(:621)。§5 表把它列进 V4,实为已就绪。

---

## 3. 未做的三块 + 规模

### 3.1 v6 分片 / 重组(~200+ LOC,大)

- **TX**:`transmit_ipv6_packet`(ether/mod.rs)现在超 MTU 直接 `EMSGSIZE`(V1a 注释:"source-side fragment headers land in V4")。IPv6 源分片 = 插入 **Fragment 扩展头**(next_header=44)+ 按 MTU 切片。要镜像 v4 `transmit_ipv4_fragments`,但用 smoltcp `Ipv6FragmentHeader`/`Ipv6FragmentRepr`(仓内 fork 已导出)。
- **RX**:`demux_ipv6`(smoltcp_demux.rs)遇 `IpProtocol::Ipv6Fragment` 会落 `_ => Unsupported` 丢掉。要镜像 v4 的 `prepare_ipv4_ingress` + `Ipv4ReassemblyEntry`(LRU/TTL,含 R3b 抗洪)+ `assemble_ipv4_packet`,但解析 Fragment 头重组。
- **规模**:v4 分片机制 ~200 LOC(结构 + 重组 LRU/TTL + TX 切片)。v6 对等 + Fragment 头处理相当。

### 3.2 `ipv6_forwarding`(~100 LOC,中)

- v4 有 `ipv4_forwarding: AtomicBool`(namespace.rs:79)+ `pending_ipv4_forwards` 队列 + 转发路径(:2230 `if !ipv4_forwarding_enabled()` … `enqueue_pending_ipv4_forward`)+ `record_forwarding`/`forwarding_outcome`。**bridge_tests 用它**(834/985/…),是 Alpine/Docker **v4** NAT 数据面(容器转发)的一部分。
- v6 对等 = 镜像:`ipv6_forwarding` 标志 + v6 转发路径(收到非本机 v6 包,查 `best_ipv6_route`/`routes6`,转发到 egress)。

### 3.3 v6 forwarding sysctl 真值(~30 LOC,小)

- 已有 `/proc/sys/net/ipv6/conf/*/disable_ipv6`(=0)、`accept_dad`(可写 no-op)(procfs mod.rs)。
- 缺 `/proc/sys/net/ipv6/conf/all/forwarding`(读 `ipv6_forwarding_enabled()`,写设置它)。只在 3.2 实现后才有意义。

---

## 4. 价值评估(为何推荐延后)

### 4.1 LTP 计分:无已知 v6 分片/转发靶

- 网络计分账本(`msp/ltp-net-official-scoring-ledger-2026-06-10-zh.md`,记忆 [[net-official-scoring-ledger]]):58 个可计分 net 文件,天花板 946。**无 v6 分片、无 v6 转发靶**。
- 唯一 `fragments` 提及 = **SCTP 自己的**分片测试(SCTP 层,非 IP 层 v6 分片),已由 SCTP 工作覆盖。
- `forwarding` 提及全是 **v4**(Docker/Alpine NAT 数据面、bridge forwarding、容器场景),无 v6 forwarding 靶。
- 可计分 v6 靶(`ipv6_lib` 四件、ping6 全族、tracepath601、tcpdump601)**V1-V2 已覆盖**。

### 4.2 实务罕见

- IPv6 源分片实务极少:PMTUD 普遍 + 1280 字节最小 MTU 保证,应用层几乎不触发 v6 分片。
- v6 转发只在**本机当 v6 路由器/网桥**时用(容器 v6 netns 互通);现场赛/LTP 未见此形态。

### 4.3 ROI 对比

- V1-V3b(外部 v6 收发链)= **高价值**(补齐审计⑩「v6 有壳无数据路径」的核心),已做完。
- V4(分片 ~200 + 转发 ~100 + sysctl ~30)= **~330 LOC,已知计分收益 0**,纯架构完整性。

---

## 5. 推荐

**延后 V4**。理由:高价值的外部 v6 路径(V1-V3b)已收官;V4 是完整性收尾,无已知计分回报,且 v6 分片是大工程。

**触发条件**(满足任一再做对应子块):
1. 出现具体 LTP/现场赛 v6 分片靶 → 做 3.1(可能像 V3b 一样,真挖下去比估计小)。
2. 需要本机当 v6 路由器/网桥(容器 v6 互通) → 做 3.2 + 3.3。
3. 想要架构与 v4 完全对称(维护/审计诉求) → 全做,但当独立低优先项。

**若你仍要做**,建议顺序按价值:先 3.2+3.3(转发 + sysctl,若有容器 v6 诉求)或先 3.1(分片,若有分片靶),**不要三块一起盲做**。

---

## 6. 实现草图(备用,若立项)

### 6.1 v6 分片
- **RX**:`ether/mod.rs` 加 `ipv6_fragments: BTreeMap<Ipv6FragmentKey, Ipv6ReassemblyEntry>` + `prepare_ipv6_ingress`(镜像 `prepare_ipv4_ingress`:解析 Fragment 头 offset/M 标志,`record_range`,`is_complete` 时 `assemble_ipv6_packet` 去掉 Fragment 头拼原始 payload)+ LRU/TTL(复用 R3b 常量思路)。`process_frame_at` 的 Ipv6 臂先过重组再 demux。
- **TX**:`transmit_ipv6_packet` 超 MTU → 切片:每片 IPv6 头 + Fragment 头(id/offset/M)+ payload 段,用 `Ipv6FragmentRepr` emit。
- 验证:大 v6 ICMP echo 分片往返(镜像 `ether_iface_fragments_and_reassembles_large_icmp_echo` v4 测试)。

### 6.2 ipv6_forwarding
- namespace.rs:`ipv6_forwarding: AtomicBool` + `set/enabled` + `pending_ipv6_forwards` + v6 转发路径(镜像 `apply`/`enqueue_pending_ipv4_forward`,查 `best_ipv6_route` 选 egress)。
- 验证:v6 netns 间转发(镜像 bridge_tests 的 v4 forwarding 用例)。

### 6.3 sysctl
- procfs:加 `/proc/sys/net/ipv6/conf/all/forwarding`(+ 可能 `/proc/sys/net/ipv6/conf/default/forwarding`)读写 `ipv6_forwarding_enabled`。

---

## 7. 验证门(若实现)

| 门 | 判据 |
|---|---|
| 编译 | `cargo xtask build --target rv64-qemu` 干净 |
| 回归 | `cargo test -p tx-subsystems` 集合差=基线(**v4 分片/转发 + v6 loopback/路由不回归**) |
| 单测 | 大 v6 报文分片往返;v6 netns 转发;sysctl 读写 |
| 功能(需真机) | QEMU 大 v6 报文 / v6 容器转发 |

---

## 附:IPv6 B 路总结(V1-V3b 已收官)

| 阶段 | 提交 | 能力 |
|---|---|---|
| V1 | a90988d0 | 对外 v6 L3 发送 + ICMPv6 RX demux |
| V2 | ccae95c8 | 动态 NDP 邻居发现 |
| V3a | 6ee95e0b | v6 路由/FIB 管理面(`ip -6 route` + `/proc/net/ipv6_route`) |
| V3b | 28a05d02 | 外部 off-link v6 路由(`decide_ipv6_route` 网关) |
| **V4** | — | **分片/转发/sysctl:建议延后(LTP 价值近零)** |

**外部 off-link v6 TCP/UDP 收发链内核侧已齐**。V4 是可选的架构完整性尾巴。
