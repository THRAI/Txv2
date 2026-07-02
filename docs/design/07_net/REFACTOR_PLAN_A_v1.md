# 网络栈重构方案 A：拥抱 smoltcp（持久 Interface + 真实时钟）

<!-- txdoc:07-NET-PLAN-A-V1 -->

**Status.** v1 (2026-06-30)，**⚠️ 已被 [`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md) 取代**——引擎模型从 A1（`SocketSet` + `iface.poll()`）修订为 asterinas-true（常驻 `Interface` 仅供 context + 自有 `SocketTable` + 手写 poll），依据见 v2「版本说明」。本文件保留作留痕，勿据此实现。

**Purpose.** 把审计（[`NET_AUDIT_v1.md`](NET_AUDIT_v1.md)，10 条 + IPv6 + 二轮 R1–R4）的全部病根，收敛到一条"拥抱 smoltcp"的重构主线，给到可讨论、可分阶段执行、每步可验证不退化的细度。

**基准.** 当前工作树 `feature-network-refactor @ fd64ba24`。所有 `file:line` 按此 HEAD；smoltcp fork 在 `external/smoltcp-asterinas`（vendored，upstream 0.11 基线 + 回填 0.12 `poll` 拆分）。

**可行性裁决（两轮底座取证）.** A 所需引擎能力——持久 `Interface`、`SocketSet`、真实时钟 `poll()`/`poll_at()`、`phy::Device`、`Loopback`、**主动 `connect` 真发 SYN**、RX 校验和——**100% 已在 fork 提供且公开导出**。无一项"fork 缺失需自研协议栈"。落地成本集中在 txKernel 侧胶水与协议层主干重建，**不构成可行性风险**。

**Companion.** [`NET_AUDIT_v1.md`](NET_AUDIT_v1.md)（问题账本，本方案每步回链其编号）；[`BUS_v1.md`](../01_substrate/BUS_v1.md)（保留的就绪/等待底座）；[`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md)。

---

## 0. 一句话

> 把 smoltcp 从"wire 编解码库 + 仅 loopback 用到的 per-socket 状态机旁路"**升格为网络引擎**：per-netns 一个**持久 `Interface` + `SocketSet`**，由 reactor 用**真实时钟**周期 `poll()`、按 `poll_at()` 安排唤醒；loopback 与物理网卡都是它的 `phy::Device` 后端；收、发、握手、重传、邻居、路由、分片、定时**全部交还 smoltcp**。txKernel 只保留它擅长的：fd/netns/identity 语义、就绪发布、syscall 阻塞骨架、netfilter/路由策略。

这一步同时拔掉 §1（时钟冻结/无重传/外部 TCP 不可用）、§3（UDP 旁路）、§4（分层崩塌）、⑩（IPv6 全断）、R1（并发碎片）、R2（老化失效泄漏）、R3a（RX 不验校验和）、R4a（epoll 注册表错配）的**共同根**。

---

## 1. 理念转变（现状 → 目标）

<!-- txdoc:07-NET-PLAN-A-SHIFT -->

| 维度 | 现状（病根） | A 目标 |
|---|---|---|
| smoltcp 角色 | wire 库 + per-socket 状态机；每次 `with_context` new 一次性 `Interface(ZERO)` 即弃（`tcp.rs:712-720`） | per-netns **持久** `Interface`，长生命周期持有邻居/路由/分片状态 |
| 时钟 | 生产恒 `Instant::ZERO`（74 处），定时器全死 | reactor 真实单调时钟（`NET_NOW_NS`）驱动 `poll()`，重传/老化/TIME-WAIT 复活 |
| socket 数据所有者 | 双份：smoltcp socket + staging `rx/tx_buffer` + loopback 直拷旁路（5 缓冲，`tcp.rs:24-33`） | **单一**：`SocketSet` 内的 handle，唯一缓冲 |
| 收发驱动 | 自研 `PollContext`（demux/backlog/accept/egress）+ `EtherIface`（ARP/分片/路由）手工实现 | `iface.poll(now, dev, &mut sockets)` 一次驱动全部 |
| loopback vs 外部 | 两套不共享抽象的平行栈（直拷 vs 手工解析旁路） | 同一 `Interface`，loopback 与 eth 只是不同 `phy::Device` |
| 就绪 | `io` 缓存 + `recv_wq`/`send_wq` 两套真相，锁外清位丢唤醒（R1a） | poll 后从 `SocketSet` 单一来源派生、电平触发、单锁 |
| IPv6 | 数据路径几乎全断（⑩） | smoltcp `Interface` 原生统一 v4/v6 邻居/路由 → 顺带复活 |

---

## 2. 目标架构

<!-- txdoc:07-NET-PLAN-A-ARCH -->

```
 syscall 层 (tx-shims)         socket(2)/connect/send/recv/poll/epoll
   │  保留：阻塞骨架 poll_ready→wait_token→wait_source::wait_on_token→await
   │  改：内联同步 drive_* → kick_poll + 让 engine poll
   ▼
 socket 身份层 (net/structure)  Cap<SocketIdentity> + SocketReadiness(RawQueue)   ← 保留
   │  SocketPayload 瘦身：9×Option<RawX> → enum SocketImpl{ Tcp(handle)|Udp(handle)|Icmp|Unix|... }
   │  identity ↔ smoltcp SocketHandle 映射
   ▼
 NetEngine (per-netns, 新建)    持久 Interface + SocketSet + per-iface phy::Device
   │  poll(now): iface.poll(now, &mut dev, &mut sockets) → diff readiness → publish_to
   │  next_deadline = iface.poll_at(now, &sockets)
   ▼
 phy::Device 后端 (适配)        SmoltcpDev<'a>{ ops: &dyn NetDeviceOps, guard: &Guard }
   │                            LoopbackDevice / virtio / veth ... (NetDeviceOps 保留)
   ▼
 reactor (tx-kernel)            单 delegate task 迭代 netns 各自 poll；deadline task 按 poll_at 唤醒
                                NET_NOW_NS = AtomicU64 ← read_ns()   ← 新建
 旁路保留：netfilter / FIB 路由 / forwarding / netlink (smoltcp 不覆盖，桥接)
```

**数据流（TX/RX 对称，都经 `iface.poll`）**：
- **TX**：`send(2)` → `socket.send_slice()`（写入 SocketSet 内 handle）→ kick engine → `iface.poll` 调 smoltcp `dispatch` → `SmoltcpDev::transmit` → `ops.transmit(frame, guard)` → 网卡/loopback。
- **RX**：网卡/loopback → `ops.receive()` → `SmoltcpDev::receive` 喂 `iface.poll` → smoltcp 解析/邻居/分片/投递到 handle → diff readiness → `publish_to(socket)` → 唤醒等待者。

---

## 3. 关键设计决策（★ = 待你拍板）

<!-- txdoc:07-NET-PLAN-A-DECISIONS -->

### ★ D1. 引擎风格：A1（SocketSet + `iface.poll`）vs A2（per-socket `process/dispatch`）
- **A1（推荐为终态）**：所有 socket 入 per-netns `SocketSet`，`iface.poll(now, dev, &mut sockets)` 一次驱动。**删代码最多**——`PollContext`（`poll_context.rs` 全文）、`EtherIface` 手工 ARP/分片/路由（`ether.rs:121-369`）全部由 smoltcp 接管。最标准、最少长期维护负担。
- **A2（过渡态）**：保留 txKernel 的 `SocketTable` demux + per-socket `process/dispatch`（fork 已 `pub`），只把"一次性 `Context`+冻结时钟"换成持久 `Interface`+真实时钟、补 `socket.connect()`。迁移最平滑，但**留着 `PollContext` 这套自研引擎**要长期维护。
- **建议**：终态选 **A1**；但**分阶段时早期阶段天然经过 A2 形态**（先持久化+解冻，再逐步把 socket 迁入 SocketSet、删 PollContext）。即 A2 是通往 A1 的脚手架，不是竞争方案。**待你确认终态是否锁定 A1。**

### ★ D2. Interface 粒度：per-netns 一个（含多 device）
- smoltcp 一个 `Interface` 绑一个 `Device`。多网卡/netns 的处理：**per-netns 一个 `Interface` + 一个聚合 `Device`**（把该 netns 的多张网卡多路复用进一个 `phy::Device`），或 **per-(netns,iface) 一个 `Interface`**。
- **建议**：首版 **per-(netns, iface) 各一个 Interface + SocketSet**，挂在 `NetNamespaceIfaceRuntime`（`namespace.rs:216-223`，现持 `&'static EtherIface`，改持 Interface）。loopback 是该 netns 的一个特殊 iface。跨 iface 路由由保留的 FIB 选 oif 后投递到对应 Interface。**待你拍：聚合单 Interface vs per-iface 多 Interface。**

### ★ D3. 时钟：全局 `NET_NOW_NS: AtomicU64`
- **矛盾（底座实证）**：真实时钟 `wall_clock::monotonic_now_ns::<P: TimeIf>()`（`wall_clock.rs:128`）**需 `P` 泛型**，只在 tx-kernel/tx-shims 可调；而 tx-subsystems 的 step 函数**是 P-无关的**，这是当前全传 `Instant::ZERO` 的根因（`step_process_network_events.rs:55,71`、`helpers.rs:365`、`tcp.rs:717`）。
- **解法（推荐）**：新建全局 `NET_NOW_NS: AtomicU64`，由 delegate task / deadline task / syscall 入口（都有 `P`）在每次进入前 `store(P::read_ns())`；tx-subsystems 的 step / `iface.poll` 路径 **P-无关地 `load()`** 取 `Instant::from_micros(NET_NOW_NS.load()/1000)`。一举解决 P-无关层取时间。
- 注：`driver.now()`（`init/net.rs:227-230`）已是真实时间，delegate task 内 `iface.poll(driver.now())` **零额外接线**；`NET_NOW_NS` 只为 syscall 内联 poll 与纯 step 层服务。**待你拍：全局 atomic vs 把 `P` 线穿到所有 step。**

### D4. poll 驱动模型：保留全局单 delegate task，迭代 netns
- 现状：**全局单个** delegate task（`runtime.rs:120-153`），经 `drive_all_net_namespace_runtimes_at`（`namespace.rs:1795`）迭代所有 netns；wake 是**单 carrier**（`queue.rs:14`）。
- **建议**：首版保留单 task，把其 POLL 分支体改为"遍历 netns → 每个 `iface.poll()`"。`drive_all_net_namespace_runtimes_at` 是天然迭代锚点。**不**首版做 per-netns task（要先拆单 carrier，牵动唤醒链，收益低）。

### D5. Device 适配：`SmoltcpDev<'a>` 借用适配器
- 新建 newtype `SmoltcpDev<'a>{ ops: &'a dyn NetDeviceOps, guard: &'a Guard<'_> }` 实现 `phy::Device`（`&mut self` 仅外壳，内部走 ops 的 `&self`）。`NetDeviceOps` trait 本体与各实现（virtio/veth/loopback/dummy）**保留**。
- `RxToken` 包 `RxFrame`（`consume(f)=f(bytes)`）；`TxToken::consume(len,f)` 分配 len、smoltcp 填充、再 `ops.transmit(&buf, guard)`。`capabilities()` 由 `device_kind()`/`mtu()`（注意以太 MTU=IP MTU+14）/`ChecksumCapabilities` 合成。
- **摩擦点**（见 §6 风险）：smoltcp `Device::transmit` 不带 guard，适配器须在 `poll()` 期持 `&'a Guard`；TX 满队列只能返 `None`，背压需 `poll_at`/device-ready 重新 kick（现 `yield_on_wait_source` 语义需外层重建）。

### ★ D6. socket 锁模型：per-netns NetEngine 单锁
- 现状：socket 逻辑状态切成 ~15 把独立锁（R1 全系列竞争根源）。
- **建议**：`NetEngine`（Interface+SocketSet）一把锁覆盖"poll + 收发 + 就绪派生"的复合不变量；socket 身份层（identity/readiness）仍可细粒度。这样 R1a/b/c/d/e 的跨锁竞争**结构性消失**（poll 与 send/recv 在同锁下串行）。代价：per-netns 串行化（但 netns 间仍并行，且 TCG 下 net 非瓶颈）。**待你拍：per-netns 大锁 vs 保留细粒度 + 仅修就绪派生。**

### D7. 就绪：poll 后从 SocketSet 单一来源派生
- poll 完，对每个 dirty handle 读 `can_recv/can_send/state/may_recv` → 与上次 diff → `publish_to(socket)`（`packet/publish.rs:27-48`）→ `fire_recv/send/accept`。**电平触发、单一真相**，消灭 R1a（锁外清位）/R1b（io 缓存丢更新）/R4d（设备路径缺自愈）。`SocketReadiness`/`wait_source`/`step_poll_*` 阻塞骨架**全保留**。

### D8. loopback：用 smoltcp `Loopback` device
- 删自研 `LoopbackIface`（`loopback.rs`）+ `step_tcp_loopback` 直拷（`step_send.rs:363-426`）+ `step_*_loopback`。netns 的 loopback 成为该 Interface 的一个 `Loopback::new(Medium::Ip)` 后端（或 IP medium + 本地地址）。loopback TCP 自此走**真握手**（smoltcp），不再手工置 `Connected`。

### D9. 路由 / netfilter / bridge / netlink：保留 + 桥接（smoltcp 不覆盖）
- smoltcp 有 `routes`（简单路由），但 txKernel 的多表 FIB（`namespace.rs:73`）、netfilter/NAT（`netfilter.rs`）、forwarding（`namespace.rs:1854`）、rtnetlink/nfnetlink 控制面 smoltcp **不管**。
- **桥接**：FIB 选出 `{oif,next_hop}` → 投递到对应 Interface + 同步 smoltcp `routes`；netfilter hook 在 `iface.poll` 前（ingress）/后（egress）施加；netlink 仍是控制面、改 FIB/iface 配置。**bridge 移出 device 层**（修 §4 泄漏）。**待讨论：NAT/conntrack 与 smoltcp 的协同点细化。**

### D10. 静态 ARP：动态优先，必要时小补丁
- fork **无公开 `add_neighbor`**。对真实 responder（QEMU 网关 10.0.2.2/DNS 10.0.2.3）走**动态 ARP**（medium-ethernet 自动）即可；若需静态注入，打小 fork 补丁暴露 `NeighborCache::fill`（`neighbor.rs`）。

### D11. ICMP / DNS：开 feature
- fork 未启 `socket-icmp`/`proto-dns`/`socket-raw`（`Cargo.toml:22`）。P2/P4 开 `socket-icmp`（ping 走 smoltcp `icmp::Socket`）、按需 `proto-dns`。**非 fork 缺失，纯开关。** 在此之前 ICMP 可暂留现有手工合成。

### D12. per-netns 生命周期：必须补 Drop 回收
- 持久 Interface/SocketSet 含大块 socket buffer 堆内存，**不能照搬** `EtherIface` 的 `Box::leak`（`namespace.rs:1668`）。`NetNamespacePayload::Drop`（`namespace.rs:307-333`）须扩展为真正回收 Interface+SocketSet（呼应 §5 的 R2 泄漏修复）。

---

## 4. 模块重组：保留 / 适配 / 重建 / 删除

<!-- txdoc:07-NET-PLAN-A-MODULES -->

**保留（复用率最高，A 的地基）**：
- reactor 接线：spawn 两 task + affinity + deadline 供给链（`init/net.rs:258-400`）、wake carrier、`kick_poll/kick_tick`（`delegate/queue.rs:39,43`）、smoltcp↔reactor 时钟换算（`delegate/timer.rs:8-19`）。
- 就绪/等待：`SocketReadiness`(`readiness.rs:30-67`)、`publish_to`(`publish.rs:27-48`)、`step_poll_*`(`step_poll.rs`)、`wait_source::wait_on_token`、syscall 阻塞骨架（`socket.rs:1055-1177,447-493`）。
- `NetDeviceOps` trait 本体 + 各实现；netfilter/FIB/netns 表/netlink 控制面。

**新建**：
- `NetEngine`（per-netns 持久 `Interface` + `SocketSet`）+ 其 Drop（D1/D2/D12）。
- `SmoltcpDev<'a>` 适配 newtype（D5）。
- 全局 `NET_NOW_NS`（D3）。
- identity ↔ `SocketHandle` 映射（`SocketTable` 增映射，保留 fd/bind/listen 语义）。

**适配（接口在位，改实现）**：
- `next_deadline`：backlog → `iface.poll_at()`（`runtime.rs:216-217,296`）。
- RX 入口：`next_packet()`→`process_frame_at` 改为喂 `iface.poll`（`init/net.rs:162-180`）。
- syscall 内联 `drive_*` → `kick_poll`+让 engine poll（`socket.rs:1056,465`、`helpers.rs:188-207,365`）。
- per-netns 迭代 poll（`namespace.rs:1795-1851`）。

**重建 / 删除**（被 smoltcp 接管）：
- ❌ `protocol/poll_context.rs` 全文（demux/backlog/accept/egress）→ `iface.poll`+`SocketSet`。
- ❌ `protocol/ether.rs:121-369` 手工 ARP/NDISC/IPv4 分片/路由/`process_frame_at`/`dispatch_ip_at` → smoltcp 邻居+分片+routes。**ether.rs 拆分（④）在此自然完成**：残余的纯帧编解码归 `phy::Device` 适配，L3 全归 smoltcp。
- ❌ `protocol/loopback.rs` `LoopbackIface` → smoltcp `Loopback`（D8）。
- ❌ `RawTcpSocket` 5 缓冲 + 手工 `process/dispatch`（`tcp.rs:24-103,427-486`）→ SocketSet handle（②⑨）。
- ❌ 双 TCP 路径（`step_process_network_events.rs:275` 直收 vs `poll_context.rs` smoltcp）统一到单 Interface。
- ♻️ `SocketPayload`：9×`Option<RawX>` → `enum SocketImpl`（⑨，与 D6 单锁一起做）。

---

## 5. 分阶段迁移（每阶段独立可验证、不退化）

<!-- txdoc:07-NET-PLAN-A-PHASES -->

> 原则：**绝不一次性重写 4 万行**。每阶段结束都能编译、跑 LTP net 不退化、可提交。早期阶段（P0/P1）经过 A2 形态，后期（P3）收敛到 A1。

### P0 — 解冻：真实时钟 + per-netns 持久 Interface 骨架
- 建 `NET_NOW_NS`（D3）；在 delegate/syscall 入口 store 真实 ns。
- 每 netns 建持久 `Interface`（替 `with_context` 一次性 iface），**socket 暂仍 per-socket**（A2 形态），但 `Context` 来自持久 iface、时钟用 `NET_NOW_NS`。
- **修**：① 时钟冻结 → 推进；R2a 老化定时器开始有意义。
- **验证**：编译 + loopback TCP/UDP LTP 不退化 + 观测 `now` 推进、smoltcp 重传定时器被 `poll_at` 排上。

### P1 — loopback 走 smoltcp Device + SocketSet
- 引入 per-netns `SocketSet`；loopback 用 smoltcp `Loopback` device；loopback TCP/UDP/connect 改走 `iface.poll`（真握手）。
- **删**：`step_tcp_loopback` 直拷、`LoopbackIface`、`PollContext` 的 loopback 部分。
- **修**：①（loopback 重传）、②（loopback 侧双份/双通路）、R1a 的 loopback 暴露面。
- **验证**：loopback TCP/UDP LTP（recv01/connect/tcp_lifecycle 等）、`tests/loopback_tests/*`。

### P2 — 外部网卡走 smoltcp（这步打通外部 TCP/IPv6）
- `SmoltcpDev` 适配 virtio/veth；RX 喂 `iface.poll`（smoltcp 解析/邻居/分片）；TX 走 smoltcp dispatch；`socket.connect()` 真发 SYN、`listen`/accept 走 smoltcp。
- **删**：`EtherIface` 手工解析/`dispatch_ip_at`/ARP；`process_tcp_event` 直收旁路。
- **修**：①（外部 TCP 结构性不可用 → 可用）、③（UDP 旁路）、R3a（RX 验校验和，device caps 开 Rx）、R4a（epoll 注册表错配——socket 就绪统一到保留的 wait-source 路径）、⑩（IPv6 数据路径随 Interface 复活，开 ipv6 已启用）。
- **验证**：git-over-HTTPS（对照 feature-network-next 已通）、外部 ping/UDP、net_stress、`-smp 4` 并发。

### P3 — socket 模型瘦身 + 单锁
- `SocketPayload`：9×Option → `enum SocketImpl`；TCP 状态/缓冲全在 SocketSet handle；`NetEngine` 单锁（D6）；就绪单一来源（D7）。
- **修**：⑨（字段）、②（双份/三处记账）、R1b–e（并发）、R2b/c/d/f（泄漏：close 排空 backlog、所属 ns 表、补 Drop、断引用环）、R2e（conntrack 加界+老化）。
- **验证**：全 LTP net 不退化 + 并发压力 + 内存（大量连接不 OOM、close 不泄漏）。

### P4 — 收尾：分层 + IPv6 + ICMP/DNS feature
- ether.rs 残余拆 `link/`（帧编解码归 Device）；开 `socket-icmp`（ping 走 smoltcp）、按需 `proto-dns`；netfilter/bridge 移出 device 层、桥接确定化（D9）。
- **修**：④（分层）、⑩（IPv6 ping6/路由/邻居）、R3b（分片表 LRU）、R3c、§4 device 泄漏、R2(B 裁决) 的 `Box::leak` 与错误 SAFETY 注释。
- **验证**：IPv6 LTP（ping6/ipv6_lib）、netns/bridge/netfilter LTP、全量对照 main。

| 阶段 | 主交付 | 修复的审计项 | 验证基线 |
|---|---|---|---|
| P0 | 真实时钟 + 持久 Interface 骨架 | ①(时钟) R2a | loopback LTP 不退化 |
| P1 | loopback 走 smoltcp + SocketSet | ① ② (loopback) | loopback TCP/UDP LTP |
| P2 | 外部网卡走 smoltcp | ① ③ ⑩ R3a R4a | git-https/net_stress/-smp4 |
| P3 | socket 瘦身 + 单锁 | ② ⑨ R1 R2 | 全 LTP net + 并发 + 内存 |
| P4 | 分层 + IPv6 + ICMP/DNS | ④ ⑩ R3b/c §4泄漏 | IPv6/netns/bridge LTP + 对照 main |

---

## 6. 风险与缓解

<!-- txdoc:07-NET-PLAN-A-RISKS -->

| 风险 | 说明 | 缓解 |
|---|---|---|
| **TCG poll 频率** | 周期 poll 若忙转会吃掉 TCG 时间预算（记忆里 net_stress 对 per-packet 成本敏感） | 严格按 `iface.poll_at()` 唤醒（事件驱动，非轮询）；无事不 poll |
| **TX 背压语义错配** | 现 device `transmit` 用 `yield_on_wait_source` 表满队列；smoltcp `transmit→Option<TxToken>` 无此概念（满返 None） | 满队列返 None + 用 device-ready/`poll_at` 重新 kick；勿丢 TX 唤醒（R4 同源，需测试覆盖） |
| **`&Guard` 贯穿 device** | smoltcp `Device` 方法不带 guard，适配器须在 poll 期持 `&'a Guard` | 确认 `iface.poll` 调用点（delegate step / syscall）guard 生命周期覆盖整个 poll |
| **per-netns 生命周期泄漏** | 持久 Interface/SocketSet 是大块堆内存，照搬 `Box::leak` 会泄漏 | D12：必须补 `Drop` 真回收（本身就是 R2 修复项） |
| **LTP 行为兼容** | 大改动可能引入 net 回归 | 分阶段 + 每阶段对照 main/feature-network-next 基线；现有 `tools/` 见证脚本 |
| **smoltcp fork 局限** | `add_neighbor` 缺、icmp/dns feature 未开 | D10 动态 ARP/小补丁、D11 开 feature；均非阻塞 |
| **单 wake carrier 限制 per-netns 并行** | 全局单 carrier | 首版单 task 迭代 netns（D4），并行优化留后续 |

---

## 7. 验证策略

<!-- txdoc:07-NET-PLAN-A-VERIFY -->

- **基线**：`main`（无网络）+ `feature-network-next`（HTTPS/外部 TCP 已通，可对照 P2）。
- **每阶段必过**：`cargo xtask unit`（host）+ 对应 LTP net 子集（QEMU）不退化、loopback TCP/UDP、`-smp 4` 并发不卡死（R1a 回归哨兵）。
- **P2 关键**：git-over-HTTPS rc=0、外部 UDP/DNS、net_stress。
- **P4 关键**：IPv6（ping6/ipv6_lib）、netns/bridge/netfilter、全量 whitelist 对照 main（沿用既有全量 LTP parity 对照方法）。
- 工具：`tools/` 现有 net 回归/见证脚本（`suite-regression-*`、`ltp-bin-witness` 等）。

---

## 8. 开放问题（讨论清单）

<!-- txdoc:07-NET-PLAN-A-OPEN -->

1. **★ D1** 终态锁定 A1（SocketSet+poll）？还是允许长期停在 A2？
2. **★ D2** Interface 粒度：per-iface 多 Interface vs per-netns 聚合单 Interface？
3. **★ D3** 时钟：全局 `NET_NOW_NS` atomic vs 把 `P` 泛型线穿到 step 层？
4. **★ D6** 锁模型：per-netns 大锁（简单、消除 R1）vs 细粒度（并行好、但要逐个修竞争）？
5. **D9** netfilter/NAT/conntrack 与 smoltcp 的协同边界（smoltcp 不覆盖，桥接点需细化）——是否值得在 A 范围内一并重做，还是先保留现有 netfilter 仅做 ingress/egress hook？
6. 范围：netlink(rtnetlink/nfnetlink ~4000 行)、SCTP/RDS/AF_PACKET 这些 smoltcp 不管的协议，A 阶段**不动**（仅保证它们与新 engine 共存），是否同意？
7. 是否需要在 P0 之前先做一个"最小可行原型"（单 netns、单 iface、loopback-only 的持久 Interface+poll）验证 TCG 性能与 guard 穿线，再决定全量推进？

---

*本方案待讨论；定稿后将据此拆分 P0–P4 的执行计划（每阶段一份 plan）。所有 `file:line` 按 `fd64ba24`。*
