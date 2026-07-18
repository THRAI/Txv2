# P4 执行计划：收尾 —— 分层拆分 + R3 输入验证 + IPv6 范围裁量 + §4 泄漏

<!-- txdoc:07-NET-P4-V1 -->

> 阶段来源：[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md) §5-P4。前序 P0-P3（时钟/loopback/外部/上半接入/瘦身/资源）全部收官，审计 ①-⑩ + R1-R4 主体落地。取证：三路并行调查（ether 分层缝、IPv6 数据路径精确复核、R3a/R3b/feature 开关）。所有 file:line 按 `55612197`。

---

## 0. 核心裁量：IPv6 范围（P4 最大的设计点，必须先拍板）

**取证推翻了审计原判**。审计⑩ 记"IPv6 数据路径几乎全断"是 **P2-S7 之前**的态。当前精确态：

- **已通且已计分**：`net.ipv6` **46/46**、`net.ipv6_lib` **76/77**（唯一缺口是 musl libc 表缺 `getprotobyname("hopopt")`，非内核）——全部走 **loopback + synthetic（configured-echo 本地合成）+ 控制面（procfs/rtnetlink/ip6tables/nft6）**。ping601/602 走 `step_send.rs:645` 的 synthetic 本地合成回环，**不是** smoltcp icmp::Socket。
- **仍断**：真·外部 v6 wire 数据路径——ether v6 TX 组帧（`build_ipv4_ethernet_frame` 硬编码 `Ipv4`，ether.rs:1016）、v6 FIB（无 `best_ipv6_route`，只 `best_ipv4_route` namespace.rs:974）、NDISC NA/NS 学习+解析读取（`ndisc_table` 只写不读作解析，ether.rs:127/407/424）、ICMPv6 wire RX（`PacketDispatch` 无 v6 变体，demux v6 臂对 ICMPv6→Unsupported）。

**判决性事实：当前零 LTP 靶依赖真外部 v6 wire**（P2 文档 REFACTOR_P2_v1.md:159/204 已把此边界记为 P4）。且 `socket-icmp` 开启不是纯开关——内核不用 smoltcp `Interface`，需写实例化/驱动胶水（取证 §5）。

**→ 建议（设计点 §7-1）：P4 IPv6 只做"零成本放行"，不做真外部 v6 wire 大工程**。真外部 v6 收发（TX 组帧/FIB/NDISC 学习/ICMPv6 RX）投产比存疑（大代码量 × 零 LTP 回报），随 P5 多 netns（届时 iface 重写）或"真机/真 v6 链路需求出现"再立项。P4 只补 demux v6 ICMP 变体这类顺手项（若判决单测能证其价值）。

---

## 1. 病根与目标（审计 ④、R3a/R3b、§4 泄漏、B 裁决）

**④ 分层崩塌**：`EtherIface`（ether.rs:121-133，1243 行）单结构体单 impl 融合 L2（ether 帧/ARP/NDISC）+ L3（IPv4 路由/转发/重组/分片）+ ICMP 编排 + 设备 TX。**取证利好**：四层方法可清晰分堆，只有 **4 个跨层方法**（`process_frame_at`/`dispatch_ip_at`/`transmit_ipv4_packet`/`maybe_reply_icmpv4`）；缝在 **ARP 缓存**（IPv4→MAC 骑跨 L2/L3）。**关键裁量**：拆成两个各自持锁的结构体会产生 link↔net 三角循环（ARP 是物理成因）——**按文件/impl 拆、保持单结构体单锁**是正确切法（对齐 D4"每 iface 一把锁"，不引入跨结构体锁序）。

**R3a RX 校验和不一致**：硬件 demux 主路径全 `new_checked`（只校长度，smoltcp_demux.rs:81/99/128 + v6 臂 42/70），loopback/ICMP 路径全 `Repr::parse` 验校验和（poll_context.rs:156/253/308）。virtio-net 未协商 CSUM offload（virtio-drivers 0.11 SUPPORTED_FEATURES 无 CSUM/GUEST_CSUM）且封装层忽略 `VirtioNetHdr.flags`（tx-drivers/virtio/net.rs:424-437）→ **无硬件兜底，损坏 UDP/TCP 事件字段/IPv4 头被当合法包投递**。**修复面小（取证）**：demux_tcp 构造的 `SmoltcpTcpSegment` 已含 `ip_repr`（tcp.rs:607-610），TCP 验证机器已在跑——只需 segment=None 时返回 Malformed 而非照发事件；UDP 补一次 `UdpRepr::parse`；IPv4 头 `new_checked`→`Ipv4Repr::parse`。地址在 demux 处已有（`ipv4.src_addr()/dst_addr()`）。

**R3b 分片表全清**：`ipv4_fragments`（ether.rs:128，上限 64）满时 `fragments.clear()` 整表全清（ether.rs:794-795）——65 个伪造源首片即冲掉所有合法在途重组（低危 DoS）。`Ipv4ReassemblyEntry`（ether.rs:170-176）无 `last_seen`。**修复**：加 `last_seen: Instant`，溢出驱逐最旧者（LRS，照抄 P3-C conntrack 范式）+ 惰性超时清扫。

**§4 device 层泄漏 + bridge 耦合**：bridge（device/bridge.rs:11-13）直调**全局** `run_frame_hook`，而 namespace 转发调**作用域** `run_frame_hook_in_namespace`（netfilter.rs:328 vs 333，层次错位铁证）；`NetDeviceOps` trait 内置 4 个 `bridge_*` 方法（device.rs:86-108）污染设备契约，仅 `BridgeDevice` 实现。设备本体/接口名/registration 全 `Box::leak`（veth.rs:155-165、bridge.rs:498-507、rtnetlink.rs:1957 等）RTM_DELLINK 只解绑不回收。

**B 裁决 SAFETY 注释错误**：namespace.rs:316-321 称 `Index` 无 Drop、`drop_in_place` 是 no-op——实则 index.rs:216-232 有真 Drop（对 COMMITTED 槽 `assume_init_drop` 键值）。行为安全（表 drop 时已空），但注释推理错、误导维护者。

---

## 2. 分步实施（S1–S5，每步独立编译/验证/提交/可回滚）

> 排序按"风险从低到高、独立性从强到弱"：先做零行为变化的顺手修（S1 SAFETY 注释 + S2 R3b），再做有判决单测的正确性修（S3 R3a），最后做大重构（S4 分层）与可选放行（S5 IPv6）。

### S1 —— B 裁决：修正 SAFETY 注释（零行为，顺手）

- namespace.rs:316-321 的错误注释改为正确表述：`Index` **有** Drop（index.rs:216），`drop_in_place` 会递归跑各 Index::drop 对已提交条目 `assume_init_drop`；安全前提是"drop 时表已空"（前段 prose 论据），非"无 Drop"。纯注释改，零行为。
- 顺手记账 §4 真泄漏（接口名/设备本体/registration 的 Box::leak）——**不修**（属 D12 per-netns Drop 回收，随 P5 多 netns 重写；P4 只在文档明确"这些是已知泄漏，进程级不回收，QEMU/LTP 无碍"）。
- **验证**：编译 + 集合差零变化（纯注释）。

### S2 —— R3b：分片表 LRU 驱逐 + 超时（照抄 conntrack 范式）

- `Ipv4ReassemblyEntry` 加 `last_seen: Instant`；`ingest_ipv4_fragment` 插入/命中刷新；满时驱逐最旧（`swap_remove` min-by last_seen）而非 `clear()`；加惰性 TTL 清扫（复用 P3-C `expire_and_cap` 范式，now 用 `net_now_instant()`）。
- **判决单测**：填满到上限 + 一个新流 → 断言只驱逐最旧一个（不是全清）；过期项在 now 推进后被剔除；正常重组不受影响。取证确认此类测试缺失。
- **验证**：单测 + loopback/外部冒烟（重组路径不退化）。

### S3 —— R3a：demux 验校验和（对齐 loopback）

- `demux_tcp`/`demux_tcp_v6`：segment 为 `None`（校验和失败）时返回 `PacketDispatch::Malformed` 而非照发事件（segment 验证机器已在跑，成本零）。
- `demux_udp`/`demux_udp_v6`：补 `UdpRepr::parse`（或复用 `UdpRxDatagram::parse_ipv4_packet`），**保留 UDP-over-IPv4 checksum==0 合法放行**语义（udp.rs:233-239）。
- `demux_ipv4`：`new_checked`→`Ipv4Repr::parse`（IPv4 头校验和自洽，无需地址）。
- **判决单测**：注入坏校验和的 TCP/UDP/IPv4 包 → 断言 demux 返回 Malformed（不投递 socket）；好校验和照常投递。取证确认此类测试缺失。
- **验证**：全冒烟（ext/tcp-lo/udp-lo/dns/seq/epoll）——真实流量校验和都是对的，必须全绿（证明没误杀合法包）；bridge_tests netfilter 不退化。
- **风险**：误杀合法包会让所有冒烟挂——冒烟矩阵是直接回归网。

### S4 —— ④ 分层：ether.rs 按文件拆（保持单结构体单锁）

- ether.rs（1243 行）拆成模块（如 `ether/link.rs` L2 帧/ARP/NDISC、`ether/l3.rs` L3 路由/重组/分片、`ether/icmp_reply.rs` echo 合成编排），但 **`EtherIface` 仍是单结构体单锁**——按 `impl` 块/自由函数归属拆文件，不拆结构体（避免 ARP 三角循环）。4 个跨层方法留在主 impl（它们本就编排多层）。
- **保 `net::protocol::` re-export 路径不变**（EtherIface/decide_ipv4_route/Icmpv4Event 等，protocol/mod.rs:11-16）——否则 tx-kernel/init/net.rs:26/47/73/240 跨 crate 引用 + 8 个测试文件 import 全要改。re-export 保住则外部零改动。
- **bridge 移出 device 层**（§4，若 S4 有余量或拆成 S4b）：`bridge_*` 从 `NetDeviceOps` 摘除，`BridgeDevice` 变独立 L2 转发器（自持 ports/learned，内部走 `run_frame_hook_in_namespace` 对齐 namespace）；设备契约回归纯 rx/tx/mac/mtu。**风险大**（触 rtnetlink/namespace 多点），可独立评估或挂 P5。
- **验证**：纯重构行为保持——集合差零变化 + 全冒烟 + bridge_tests + ether/icmp 测试（import 若改则同步）；la64/boot。
- **回滚**：S4 是纯文件移动 + import 调整，单 commit revert 即回退。

### S5 —— IPv6 零成本放行（可选，视 §0 拍板）

- **仅当 §7-1 拍板"放行"**：demux 加 `PacketDispatch::Icmpv6` 变体 + v6 臂 ICMPv6 分派（若能写判决单测证其价值）；`build_ipv6_ethernet_frame`（0x86dd）+ `dispatch_ipv6_at` 骨架。
- **默认不做真外部 v6 TX/FIB/NDISC**（大工程零 LTP 回报，挂 P5/需求驱动）。
- **验证**：v6 demux 判决单测；不引入回归。

---

## 3. 测试方案

1. **判决单测（本波核心，取证确认缺失）**：S2 分片 LRU（填满驱逐最旧/超时剔除）；S3 坏校验和被拒（TCP/UDP/IPv4）；S5（若做）v6 ICMP demux。
2. **回归网**：host 集合差（P3-C 基线 + 已知新测试名）；六冒烟（ext/tcp-lo/udp-lo/dns/seq/epoll）+ accept + bulk32K——**S3 的直接回归网**（真实流量校验和都对，误杀即挂）；bridge_tests netfilter（S4 分层/bridge 移出的回归网）；ether_iface_arp/icmp 测试（S4 import）；busybox-boot + la64。
3. **暂缺**：LTP net 全量（无镜像）；真外部 v6 wire（§0 不做）——挂环境轮/需求驱动。

---

## 4. 风险与已知坑

1. **S3 误杀合法包**：demux 验校验和若与 loopback 语义有细差（UDP checksum==0、IPv4 分片），会让冒烟全挂——冒烟矩阵是直接回归网，误杀立现；UDP-0 语义必须照抄 loopback。
2. **S4 分层的 re-export**：拆文件时 `net::protocol::` 再导出路径是跨 crate（tx-kernel）+ 8 测试的生命线，必须保住；宁可模块名不变只挪文件内容。
3. **S4 bridge 移出 device 层风险大**：触 rtnetlink/namespace 多点 + 5 种设备的 trait 契约——建议先做 ether 拆分（S4a），bridge 移出（S4b）独立评估、撞上再做或挂 P5。
4. **回滚单元 = S 步**；S1/S2/S3 相互独立可分别 revert；S4 纯文件移动可整体 revert。

---

## 5. 设计点拍板（按既定授权取推荐）

| # | 问题 | 取向 |
| - | --- | --- |
| 1 | **IPv6 范围** | **只零成本放行（S5 可选），不做真外部 v6 wire**——当前零 LTP 靶依赖它，大代码量零回报；随 P5/需求驱动立项。socket-icmp 不开（非纯开关，需 Interface 胶水） |
| 2 | ether 分层形态 | **按文件拆、保单结构体单锁**——ARP 三角循环使拆结构体得不偿失；保 net::protocol re-export |
| 3 | bridge 移出 device 层 | **拆成 S4b 独立评估**——风险大触多点；先做 ether 拆分（S4a），bridge 撞上再做或挂 P5 |
| 4 | §4 Box::leak 真泄漏 | **记账不修**——属 D12 per-netns Drop（P5 重写）；P4 只文档明确"已知进程级泄漏，QEMU/LTP 无碍" |
| 5 | R3a 范围 | **v4+v6 demux 全验**（TCP 零成本/UDP 补 parse/IPv4 头补 parse），保 UDP-0 语义 |

---

## 6. P4 之后的余账（全景）

P4 完成后网络重构主体全部落地，剩余均已记录、各有归属：
- **真外部 v6 wire**（TX 组帧/FIB/NDISC 学习/ICMPv6 RX）：P5 或需求驱动
- **R1e bind 原子 / 多核并发实证 / net_stress 内存压力 / D14b 等待收敛**：环境轮（LTP 镜像 + 稳定多核）
- **R1c FSM 双读**：记账不改（后果小）
- **socket-icmp/proto-dns 内核 resolver**：可选增强，非阻塞（现手搓 UDP + 用户态解析已通 DNS/getaddrinfo）
- **§4 Box::leak / bridge 移出 / D4 每 iface 锁 / 多 netns**：P5

---

*P4 完成后：ether.rs 分层清晰、RX 输入验证与 loopback 一致、IPv6 边界明确记录；P0-P4 五阶段收官，asterinas-true 网络栈重构主体完成。*
