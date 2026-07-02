# 网络栈重构方案 v2：asterinas-true（常驻 Interface 供 context + 自有 SocketTable + 手写 poll）

<!-- txdoc:07-NET-PLAN-A-V2 -->

**Status.** v2 (2026-06-30)。**已收敛的设计方案**——四个主轴已拍板（见下），可据此拆 P0 执行计划。非实现。

**Purpose.** 把审计（[`NET_AUDIT_v1.md`](NET_AUDIT_v1.md)，10 条 + IPv6 + 二轮 R1–R4）的全部病根，收敛到一条重构主线，给到可分阶段执行、每步可验证不退化的细度。

**基准.** 工作树 `feature-network-refactor @ fd64ba24`。所有 `file:line` 按此 HEAD；smoltcp fork 在 `external/smoltcp-asterinas`（vendored，upstream 0.11 + 回填 0.12 `poll` 拆分）。

**Companion.** [`NET_AUDIT_v1.md`](NET_AUDIT_v1.md)（问题账本，本方案每步回链其编号）；`msp/tx-kernel-network-stack-design-v9.md`（**用户原始设计 v9**，§8/§9/§10/§16——本方案证实它本就正确，重构=把实现拉回它）；参照内核 `/home/msp/learning/asterinas`（`kernel/libs/aster-bigtcp`，本方案模型的实测来源）。

---

## 版本说明：v2 为什么推翻 v1（留痕）

<!-- txdoc:07-NET-PLAN-A-V2-WHY -->

v1（[`REFACTOR_PLAN_A_v1.md`](REFACTOR_PLAN_A_v1.md)，保留作留痕）的引擎模型是 **A1：拥抱 smoltcp 的 `Interface` + `SocketSet` + `iface.poll()`**。经 asterinas 源码 + smoltcp fork 源码**亲验后推翻**，换为 **asterinas-true**。两条硬证据：

1. **smoltcp 的 `SocketSet`/`poll()` 本身就是 O(n)，没有 demux 加速可继承。** `SocketSet` 是平铺数组、无端口/四元组索引（`socket_set.rs:44-46`）；每收一个包要线性扫所有 socket 逐个 `accepts()`（`iface/interface/tcp.rs:21-30`）；`poll()` = O(socket×包)，egress/poll_at 每轮再各全扫一遍（`mod.rs:447-453 / 654-755 / 536-549`）。LTP 几十连接无所谓，真机几百上千连接是天花板。
2. **asterinas（fork 来源）刻意不用 `SocketSet`/`poll()`。** `aster-bigtcp/src/lib.rs:3-11` 自述：smoltcp "designed for embedded where socket count is small … **cannot satisfy general-purpose OS in flexibility and efficiency**"，于是它**自建 `SocketTable` 四元组哈希 + 手写 `poll_ingress/poll_egress` + 真实 jiffies 时钟**，smoltcp **只**做 per-socket `process/dispatch/accepts/connect/poll_at`。fork 把这些方法改 `pub`，目的正是支撑这套手动驱动。

**而这恰好就是用户 v9 设计 §9.1 的原话**（"不用 `Interface::poll()`，我们自己轮询；不用 `SocketSet`，我们用 `SocketTable`"）。所以 v2 不是新发明，是**三方合一**：用户原设计 = asterinas 实测 = smoltcp 源码结论。

> **结论**：v1 的 A1 会一头撞进 O(n) 坑并重塑用户精心设计的 SocketTable/身份模型；v2 忠于原设计，且把改动收成"外科手术式修 5 处偏离"而非"删半个引擎让 smoltcp 接管"。

|                        | v1（A1，弃）                      | v2（asterinas-true，采纳）                                            |
| ---------------------- | --------------------------------- | --------------------------------------------------------------------- |
| socket 容器            | smoltcp`SocketSet`              | **自有 `SocketTable` 四元组哈希（保留）**                     |
| poll 驱动              | `iface.poll(now, dev, sockets)` | **手写 `poll_ingress/egress`（保留 EtherIface/PollContext）** |
| Interface 角色         | 引擎主体                          | **仅供 `context_mut()`（校验和能力）+ 真实时钟**              |
| L2/L3（ARP/IPv4 解析） | 交给 smoltcp，删 EtherIface       | **保留手写（=asterinas 做法），只拆分+修时钟**                  |
| EtherIface/PollContext | ❌ 删                             | ♻️**保留 + 修**（解冻 + 恢复 `process()` 委托）             |
| smoltcp 职责           | 收发/握手/邻居/路由/分片全包      | **只做 per-socket TCP/UDP 状态机**                              |

---

## 0. 一句话

<!-- txdoc:07-NET-PLAN-A-V2-ONELINE -->

> txKernel **持有一个常驻 `Interface`**，只用它给 smoltcp 的 per-socket 状态机供 `context_mut()` 和**真实时钟**；**保留**自己的 `SocketTable`（四元组哈希 O(1)）+ 手写 `poll_ingress/egress` + `NetDeviceOps` 设备层 + FIB/netfilter 路由策略。smoltcp **只**负责单条连接的 TCP/UDP 状态机（`process/dispatch/accepts/connect/poll_at`）。删掉 5 处偏离：**一次性 `Interface(ZERO)`（冻结时钟）、RawTcpSocket 5 缓冲、loopback 直拷旁路、RX 手写 seq/ack 旁路、9×`Option<RawX>` 平铺**。

这 5 处偏离的**共同源头**是一句**设计漏写**：v9 §9.1 说对了"不调 `poll()`"，却没写"**仍须持有一个常驻 `Interface` 提供 `context_mut()`+时钟**"——实现就填成每次 new 一次性 `Interface(Instant::ZERO)`，时钟冻死，连锁出 ①③⑩ R2a。补上这句、解冻，是 P0。

> **注：以上仅"下半·引擎"战线。** 另有同等重要、可独立推进的**上半·接入**战线——让 socket 成为一等文件对象：实现 `FileOps` trait 取代 syscall 层 `if 是 socket` 特判（⑤）、把 park 从 legacy `WaitToken`/`yield_now` 忙让步收敛到 `await_wait_source`（⑥/R4a）。见 §1-bis「两条战线」+ D13/D14。**v1 与 v2 初稿都漏写了这两条**（它们不属引擎四主轴），本次补回。

---

## 1. 理念转变（现状 → 目标）

<!-- txdoc:07-NET-PLAN-A-V2-SHIFT -->

| 维度                  | 现状（病根）                                                                                                | v2 目标（asterinas-true）                                                                                                              |
| --------------------- | ----------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| smoltcp 角色          | wire 库 + per-socket 状态机；每次`with_context` new 一次性 `Interface(ZERO)` 即弃（`tcp.rs:712-720`） | **per-netns 一个常驻 `Interface`**，仅供 `context_mut()`（校验和能力）+ `now`；smoltcp 只跑 per-socket 状态机              |
| 时钟                  | 生产恒`Instant::ZERO`，定时器全死                                                                         | `NET_NOW_NS` 真实单调时钟每轮戳进 `iface.context_mut().now`，重传/老化/TIME-WAIT 复活                                              |
| TCP socket 数据所有者 | 双份：smoltcp socket + staging`rx/tx_buffer` + loopback 直拷（5 缓冲，`tcp.rs:24-33`）                  | **单一**：`Box<smoltcp::tcp::Socket>` 自带的 ring，TcpConnection 只加几个 flag（=v9 §9.3 薄封装）                             |
| 收发驱动              | 自研`PollContext`（demux/backlog/accept/egress）+ `EtherIface`（ARP/IPv4 解析）                         | **保留这套手写引擎**（=asterinas `iface/poll.rs`/`phy/ether.rs`），只解冻时钟 + RX 恢复走 `socket.process()`               |
| RX 投递               | 命中连接后手写 seq/ack 推进，**绕过** smoltcp 状态机（R3a 还不验校验和）                              | `socket.process(iface.context_mut(), ip_repr, tcp_repr)` 交还状态机；校验和由 context caps 验                                        |
| socket 容器           | 自有`SocketTable`（已是四元组哈希）                                                                       | **保留**（不引入 smoltcp `SocketSet`——它 O(n)）                                                                              |
| loopback vs 外部      | 两套平行栈（直拷 vs 手工解析旁路）                                                                          | 同一手写 poll 路径，loopback 与 eth 只是不同`NetDeviceOps` 后端                                                                      |
| socket 模型           | `SocketPayload` 9×`Option<RawX>` 平铺                                                                  | `enum SocketImpl`；StreamSocket 内部 `enum State{Init/Connecting/Connected/Listen}`（=v9 §8 = asterinas `stream/mod.rs:57-78`） |
| 就绪                  | `io` 缓存 + `recv_wq`/`send_wq` 两套真相，锁外清位丢唤醒（R1a）                                       | poll 后从 socket 状态**单一来源**派生、电平触发、每 iface 一把短锁                                                               |
| IPv6                  | 数据路径几乎全断（⑩）                                                                                      | 常驻`Interface` + smoltcp `process` 原生统一 v4/v6 邻居/路由 → 复活                                                               |

---

## 1-bis. 两条战线：下半·引擎 + 上半·接入（完整覆盖审计 ①–⑩）

<!-- txdoc:07-NET-PLAN-A-V2-FRONTS -->

本方案覆盖审计**全部 10 条**怀疑。它们分成两条**相对独立、可并行推进**的战线——之所以要显式点出，是因为 v1／v2 初稿只写了"引擎"那半条，把使用者亲点的 ⑤⑥ 漏在了外面：

**① 下半·引擎**（smoltcp 被降格的问题，= 四主轴 D1–D4）——常驻 `Interface` + 真实时钟 + 自有 `SocketTable` + 手写 poll。修 **① 时钟 / ② 双份缓冲 / ③ UDP 旁路 / ④ 分层 / ⑨ 字段 / ⑩ IPv6 + R1–R3**。已在 §3 D1–D4、§5 P0–P2 展开。

**② 上半·接入**（socket 融入统一 fd/file 抽象）——让 socket 成为**一等文件对象**，与其余 fd 子系统同构。修两条使用者亲点、初稿漏写的怀疑：

- **⑤ 无文件系统接口**（audit §5.1，◐ 分发层证实）：`OpenFile::step_read/step_write` 对 socket **主动返回 `EINVAL`**（`vfs/execution.rs:396-399,649-652`），把 socket I/O 逼到平行的 `sendto/recvfrom`；`write/read/ioctl/ppoll/pselect/epoll/splice/close` 全靠 syscall 层 `if 是 socket` 特判（`io.rs:1946/2229/1100/1497`、`fs_basic.rs:1516/1199`、`epoll.rs:119/188/255`、`splice.rs:69`）。框架**本有多态能力**——`CharDevice(binding) => binding.ops.read(...)`（`vfs/execution.rs:382,636`），socket 却在同一位置选 `EINVAL`。→ **D13**：引入与 `CharDeviceBinding.ops` 同形的 `FileOps` trait，socket 实现它、删全部特判（= Linux `socket_file_ops`／asterinas `Socket: FileLike`）。
- **⑥ wait 接入与其他子系统不同**（audit §5.2，◐ 部分证实）：net/socket 是全栈里**唯一**停在 legacy `WaitToken`+`yield_now` 忙让步的 fd 子系统（`net/execution/mod.rs:97-131`、`socket/helpers.rs:1703-1707`、`socket.rs:877/883/892/1078`）；其余 **8 个**（eventfd/timerfd/signalfd/epoll/aio/userfaultfd/ipc/proc）已迁 `await_wait_source`。底层 `WaitSource` 注册表其实统一——**底层统一、接入分裂**。→ **D14**：socket park 收敛到 `await_wait_source`，连带修 **R4a**（epoll 对 socket 无法阻塞的注册表错配）。

> **两条战线的根其实相通**：socket 一旦是一等文件对象（⑤），其 read/write/poll 走统一 trait，就绪/等待自然与其他 fd 同构（⑥ + D7 + R4a 一并落地）。**上半战线跨 VFS/shim 层**（不止 `net/`）、与引擎战线（D1–D4）相对解耦——理论上可先行或并行，但因都围绕"socket 模型"，本方案把它收敛在同一阶段 **P3** 协同（见 §5）。

---

## 2. 目标架构

<!-- txdoc:07-NET-PLAN-A-V2-ARCH -->

```
 syscall 层 (tx-shims / VFS)    socket(2)/connect/send/recv/poll/epoll
   │  【上半接入】改①：删 socket 特判(io/fs_basic/epoll/splice) → FileOps trait 多态 (D13/⑤)
   │  【上半接入】改②：park 从 legacy WaitToken/yield_now → await_wait_source (D14/⑥，同其余 8 fd 子系统)
   │  【下半引擎】改③：内联同步 drive_*/直拷 → kick engine，让手写 poll 推进
   │  保留：阻塞骨架 poll_ready→wait_source→await
   ▼
 socket 身份层 (net/structure)  Cap<SocketIdentity> + SocketReadiness(RawQueue)   ← 保留
   │  SocketPayload 瘦身：9×Option<RawX> → enum SocketImpl{ Tcp|Udp|Icmp|Unix|... }
   │  StreamSocket 内部 enum State{ Init|Connecting|Connected|Listen }  (=v9 §8 / asterinas)
   ▼
 NetEngine (per-netns；单 netns 先行)
   │  · 常驻 Interface  ──仅供──▶  context_mut()（校验和能力）+ now（真实时钟）
   │  · 自有 SocketTable（四元组哈希 O(1)）   ← 保留，不用 smoltcp SocketSet
   │  · 手写 poll_ingress / poll_egress       ← 保留 EtherIface/PollContext，不用 iface.poll()
   │      RX: device.receive → 解析 eth/ARP/IPv4 → SocketTable.lookup → socket.process(cx, ip,tcp)
   │      TX: 脏 socket → socket.dispatch(cx, emit) → 串行化(校验和) → device.transmit
   │  · 每 iface 一把短锁（smoltcp &mut self 单线程 = 结构性，Q6）
   ▼
 设备层 (NetDeviceOps, 保留)    virtio-net / loopback / veth ...
   ▼
 reactor (tx-kernel)            单 delegate task 迭代 netns→各 iface poll；deadline 按 poll_at 唤醒
                                NET_NOW_NS = AtomicU64 ← read_ns()（P-无关层取真实时钟）
 旁路保留：netfilter / FIB 路由 / forwarding / netlink（smoltcp 不覆盖，桥接）
 ── 后续阶段（非 P0–P4）：多 netns → per-netns Interface 集 + veth/bridge 跨 netns 转发 + poll 调度
```

**数据流（RX/TX 都经手写 poll，不经 `iface.poll`）**：

- **RX**：网卡/loopback → `ops.receive()` → 手写解析 eth/ARP/IPv4（保留）→ `SocketTable.lookup_connection`（O(1)）→ `socket.process(iface.context_mut(), &ip_repr, &tcp_repr)`（**交还 smoltcp 状态机**）→ diff 就绪 → `publish_to(socket)` → 唤醒等待者。
- **TX**：`send(2)` → `socket.send_slice()`（写 smoltcp socket 自带 ring）→ kick engine → poll_egress 遍历脏 socket → `socket.dispatch(iface.context_mut(), emit)`（smoltcp 生成段 + 校验和）→ `ops.transmit(frame, guard)`。

---

## 3. 关键设计决策（四主轴已拍板）

<!-- txdoc:07-NET-PLAN-A-V2-DECISIONS -->

### ✅ D1. 引擎模型 = asterinas-true（已定）

- 常驻 `Interface`（供 `context_mut()`+时钟）+ **自有 `SocketTable`**（不用 `SocketSet`）+ **手写 `poll_ingress/egress`**（不用 `iface.poll()`）+ smoltcp 只做 per-socket `process/dispatch/accepts/connect`。
- **依据**：见"版本说明"——SocketSet/poll() 是 O(n)（`socket_set.rs:44-46` 等），asterinas 与 v9 §9.1 皆刻意拒绝。**v1 的 A1 作废。**

### ✅ D2. Interface 粒度 = 单 netns 先行（已定）

- 首版只做**默认 netns**：`lo` + 一张 `eth0` = 1–2 个 Interface。多 netns（per-netns Interface 集）**留作后续阶段**（见 §5 末）。
- **依据（用户决定 + 讨论）**：多 netns 的难点**不是多核**（多 Interface 各有各锁，反而帮并行），而是 ① **veth/bridge 跨 netns 转发**（smoltcp 不知 netns/veth/bridge，配对+转发全是我们的码——LTP netns 测试考的就是这，也最可能是旧代码乱的地方）、② 多 iface 的 poll 调度、③ 资源/生命周期翻倍（R2f 引用环）、④ fd→netns→iface 查找间接层。asterinas 干脆没做 netns（全局单 `IFACES`+FIXME）。**先把核心在最简拓扑跑通；旧 per-netns 代码重构时大概率整段重写，不必保留。**

### ✅ D3. 时钟 = 全局 `NET_NOW_NS: AtomicU64`（已定）

- **矛盾（底座实证）**：真实时钟 `wall_clock::monotonic_now_ns::<P: TimeIf>()`（`wall_clock.rs:128`）**需 `P` 泛型**，只在 tx-kernel/tx-shims 可调；tx-subsystems 的 step 是 **P-无关**的 → 这是当前全传 `Instant::ZERO` 的根因。
- **解法**：全局 `NET_NOW_NS: AtomicU64`，由 delegate/deadline/syscall 入口（都有 `P`）进入前 `store(P::read_ns())`；P-无关的 poll 路径 `load()` 后 `Instant::from_micros(NET_NOW_NS.load()/1000)` 戳进 `iface.context_mut().now`。
- 注：`driver.now()`（`init/net.rs:227-230`）已是真实时间，delegate task 内零额外接线；`NET_NOW_NS` 专为 syscall 内联与纯 step 层服务。

### ✅ D4. 锁模型 = 每 iface 一把短锁（已定）

- **依据**：smoltcp `Interface`/socket 全是 `&mut self`、零内部锁（Q6）→ "一 iface 一锁"是**结构性**，非选择。该锁覆盖"poll + 收发 + 就绪派生"复合不变量 → R1a/b/c/d/e 跨锁竞争**结构性消失**。
- 单 netns 下只有 1–2 把，无需 per-netns 大锁；"按 netns 分片求并行"随多 netns 阶段再做。

### D5. poll 驱动：保留全局单 delegate task，迭代 netns→iface

- 现状全局单 delegate task（`runtime.rs:120-153`）经 `drive_all_net_namespace_runtimes_at`（`namespace.rs:1795`）迭代。首版保留单 task，POLL 分支体改为"遍历 netns → 每 iface 手写 poll_ingress/egress"。**不**首版做 per-netns task（牵动单 carrier 拆分，收益低）。

### D6. 设备层：保留 `NetDeviceOps`，手写 poll 直接调它

- 因为我们**手写** poll（不调 `iface.poll`），**不需要** v1 设想的 `SmoltcpDev: phy::Device` 适配器——poll_ingress 直接 `ops.receive()`、poll_egress 直接 `ops.transmit(frame, guard)`。`NetDeviceOps` trait 本体与各实现（virtio/veth/loopback）**全保留**。
- 校验和：`emit` 串行化 IpRepr/TcpRepr 时用 `iface.context().checksum_caps()`；网卡硬件算校验和则 device caps 报 `Tx/None`（真机阶段）。

### D7. 就绪：poll 后从 socket 状态单一来源派生

- poll 完，对每个脏 socket 读 `can_recv/can_send/state/may_recv` → 与上次 diff → `publish_to(socket)`（`packet/publish.rs:27-48`）→ `fire_recv/send/accept`。**电平触发、单一真相**，消灭 R1a（锁外清位）/R1b（io 缓存丢更新）。`SocketReadiness`/`wait_source`/`step_poll_*` 阻塞骨架**全保留**。
- **等待机制归 D14**：D7 只管"就绪何时触发"（单一来源、电平触发）；"用什么机制挂起"（`await_wait_source` 取代 legacy `WaitToken`/`yield_now`）及 R4a epoll 注册表错配，见 **D14**。就绪(D7) + 等待(D14) 配套。

### D8. loopback：当作同一 poll 路径上的一个设备

- 删自研 `LoopbackIface`（`loopback.rs`）+ `step_tcp_loopback` 直拷（`step_send.rs:363-426`）。netns 的 `lo` 成为一个 loopback `NetDeviceOps`（或 smoltcp `Loopback::new(Medium::Ip)`）后端，走与 eth 相同的手写 poll。loopback TCP 自此走**真握手**（smoltcp `process/dispatch`），不再手工置 `Connected`。

### D9. 路由 / netfilter / bridge / netlink：保留 + 桥接（smoltcp 不覆盖）

- smoltcp 不管 txKernel 的多表 FIB（`namespace.rs:73`）、netfilter/NAT（`netfilter.rs`）、forwarding（`namespace.rs:1854`）、rtnetlink/nfnetlink。**桥接**：FIB 选 `{oif,next_hop}` → 投递到对应 iface；netfilter hook 在手写 poll 的 ingress 前/egress 后施加；netlink 仍是控制面改 FIB/iface 配置。**bridge 移出 device 层**（修 §4 泄漏）。**待细化**：NAT/conntrack 与逐 socket `process` 的协同点。

### D10. 静态 ARP：动态优先，必要时小补丁

- 对真实 responder（QEMU 网关 10.0.2.2/DNS 10.0.2.3）走**动态 ARP**（medium-ethernet 自动学）即可。手写 poll 的 ARP 缓存本就在 EtherIface（`ether.rs` ARP 表，保留）；无需 smoltcp `add_neighbor`。

### D11. ICMP / DNS：开 feature（P4）

- fork 未启 `socket-icmp`/`proto-dns`/`socket-raw`（`Cargo.toml`）。P4 开 `socket-icmp`（ping 走 smoltcp `icmp::Socket`）、按需 `proto-dns`。**非 fork 缺失，纯开关。** 此前 ICMP 暂留现有手工合成（`ether.rs` echo reply）。

### D12. per-netns 生命周期：必须补 Drop 回收

- 常驻 Interface + 每 socket 的 smoltcp ring 是大块堆内存，**不能照搬** `EtherIface` 的 `Box::leak`（`namespace.rs:1668`）。`NetNamespacePayload::Drop`（`namespace.rs:307-333`）须扩展为真回收 Interface + 所有 socket（呼应 R2 泄漏修复）。

### ✅ D13. socket 成一等文件对象：引入 `FileOps` trait（⑤，上半接入战线，方向已定）

- **病根**：无 `file_operations` 式多态。`OpenFile::step_read/step_write` 对 socket **主动返回 `EINVAL`**（`vfs/execution.rs:396-399,649-652`），把 socket I/O 逼到平行 `sendto/recvfrom`；`write/read/ioctl/ppoll/pselect/epoll/splice/close` 全靠 syscall 层 `if 是 socket` 特判（`io.rs:1946/2229/1100/1497`、`fs_basic.rs:1516/1199`、`epoll.rs:119/188/255`、`splice.rs:69`）。
- **框架本有多态能力**：`StructPayload::CharDevice(binding) => binding.ops.read(...)`（`vfs/execution.rs:382,636`）——字符设备走 trait ops，socket 本可同挂一组 ops，却在该位置选 `EINVAL`。
- **解法**：给 `StructPayload`（或 `OpenFile`）引入与 `CharDeviceBinding.ops` **同形**的 `FileOps` trait（read/write/poll/ioctl/close），socket 实现它；`step_read/write` 的 socket arm 从 EINVAL 改**委派 ops**；**删除 `io.rs/fs_basic.rs/epoll.rs/splice.rs` 全部 socket 特判**。= Linux `socket_file_ops`（`f_op` 虚表，VFS 永不判"是不是 socket"）= asterinas `Socket: FileLike`。
- **注**：这是**跨 VFS/shim 层**的改动（不止 `net/`），与引擎战线（D1–D4）相对解耦；但与 D7 就绪、D14 wait 同属"socket 融入 fd 抽象"，收敛在 **P3** 协同。
- ℹ️ `feature-network-next` 已走半步（socket arm EINVAL→转发、特判收窄到 netlink），但仍是 enum-arm 转发而非 trait 多态，且不在本分支——可借鉴不可照搬。

### ✅ D14. socket 等待收敛到 `await_wait_source`（⑥ / R4a，上半接入战线，方向已定）

- **病根**：net/socket 是全栈 fd 子系统里**唯一**停在 legacy `WaitToken`+`yield_now` 忙让步的：`net/execution/mod.rs:97-131` 构造 `socket_{recv,send,accept}_wait_token`、`socket/helpers.rs:1703-1707 wait_on_yield_shape` 内部仍 `WaitToken::new`、`socket.rs:877/883/892/1078` 大量 `yield_now().await` 反复让出重试（非真挂起）。`wait.rs:1-6` 注释**自承** `WaitToken` 为 legacy、应收敛。其余 **8 个** fd 子系统（eventfd/timerfd/signalfd/epoll/aio/userfaultfd/ipc/proc）已迁 `await_wait_source`。
- **底层其实统一**：等待载体是 substrate `RawQueue`（`readiness.rs:30-34`）+ 统一 `WaitSource` 注册表——**底层统一、接入分裂**（⑥ 裁定 ◐ 部分证实，非私有原语）。
- **解法**：socket park 从 `WaitToken`/`wait_on_yield_shape`/`yield_now` 收敛到 `await_wait_source`，与 eventfd/epoll 对齐；删 net 侧 legacy `WaitToken` 构造。**连带修 R4a**：epoll 对 socket 无法阻塞的注册表错配（carrier 注册 subsystems 表 vs epoll 查 substrate 表）随统一接入消解。
- **与 D7 关系**：D7 定"就绪何时触发（单一来源、电平）"，D14 定"怎么挂起等唤醒"——就绪 + 等待配套，一起在 P3 落地。

---

## 4. 模块重组：保留 / 修 / 删除（外科手术式）

<!-- txdoc:07-NET-PLAN-A-V2-MODULES -->

> 与 v1 最大不同：**EtherIface/PollContext 这套手写引擎是保留+修，不是删**——它就是 asterinas 的 `poll.rs`/`ether.rs`，是对的。删的只是 5 处具体偏离。

**保留（A 的地基，复用率最高）**：

- 手写引擎骨架：`PollContext`（demux/backlog/accept/egress 迭代）、`EtherIface`（ARP 缓存、eth/IPv4 解析）——**修而非删**。
- 自有 `SocketTable`（四元组哈希 demux）。
- reactor 接线：spawn task + affinity + deadline 供给链（`init/net.rs:258-400`）、wake carrier、`kick_poll/kick_tick`、smoltcp↔reactor 时钟换算（`delegate/timer.rs:8-19`）。
- 就绪/等待：`SocketReadiness`（`readiness.rs:30-67`）、`publish_to`（`publish.rs:27-48`）、`step_poll_*`、`wait_source`、syscall 阻塞骨架（`socket.rs:1055-1177`）。
- `NetDeviceOps` trait + 各实现；netfilter/FIB/netns 表/netlink 控制面。

**新建**：

- 每 netns 一个**常驻 `Interface`**（替 `with_context` 一次性 iface），挂在 `NetNamespaceIfaceRuntime`（`namespace.rs:216-223`）。
- 全局 `NET_NOW_NS`（D3）。
- identity ↔ TcpConnection/UdpSocket 的瘦封装（`Box<smoltcp socket>`+flag）。
- per-netns `Drop` 回收（D12）。
- **`FileOps` trait**（同形 `CharDeviceBinding.ops`：read/write/poll/ioctl/close）+ socket 的 ops 实现（D13，上半接入，跨 VFS/shim 层）。

**修（接口在位，改实现）**：

- ⚠️ `tcp.rs:712-720` `with_context` 一次性 `Interface(ZERO)` → 借常驻 Interface 的 `context_mut()` + `NET_NOW_NS`（**解冻，①③⑩ R2a 的总修复**）。
- ⚠️ RX 命中后手写 seq/ack → `socket.process(cx, ip_repr, tcp_repr)`（**交还状态机，③ R3a**）。
- ⚠️ TX → `socket.dispatch(cx, emit)` 串行化+校验和。
- `next_deadline`：backlog 轮询 → 聚合各 socket `poll_at()`（`runtime.rs:216-217,296`）。
- syscall 内联 `drive_*` → `kick_poll`+让 engine poll（`socket.rs:1056,465`、`helpers.rs:188-207`）。
- per-netns 迭代 poll（`namespace.rs:1795-1851`）。
- **上半接入**：`vfs/execution.rs:396-399,649-652` socket arm `EINVAL` → 委派 `FileOps`（D13/⑤）；socket park `net/execution/mod.rs:97-131`、`helpers.rs:1703-1707`、`socket.rs:877-1078` 的 `WaitToken`/`yield_now` → `await_wait_source`（D14/⑥/R4a）。

**删除（引擎 5 处偏离 + 接入层特判）**：

- ❌ `RawTcpSocket` 5 缓冲 + 手工 buffer 管理（`tcp.rs:24-103`）→ `Box<smoltcp tcp::Socket>` 自带 ring（②⑨）。
- ❌ loopback 直拷旁路 `step_tcp_loopback`（`step_send.rs:363-426`）+ `LoopbackIface`（`loopback.rs`）→ loopback 当设备走手写 poll（D8）。
- ❌ 双 TCP 路径（`step_process_network_events.rs:275` 直收 vs PollContext smoltcp）→ 统一单路径。
- ❌ RX 手写 seq/ack 旁路 → `socket.process()`（见"修"）。
- ♻️ `SocketPayload` 9×`Option<RawX>`（`payload.rs:33-53`）→ `enum SocketImpl`（⑨，与 D7 一起）。
- ♻️ `ether.rs` 融合 L2/L3/ICMP（`ether.rs:121-133`）→ P4 拆 `link/`（L2 帧）/ `net/`（L3）（④），**逻辑保留，只分文件**。
- ❌ **syscall 层全部 socket 特判**（`io.rs:1946/2229`、`fs_basic.rs:1516`、`epoll.rs:119`、`splice.rs:69` 等）→ `FileOps` trait 多态（⑤，D13，上半接入）。

---

## 5. 分阶段迁移（单 netns 先行；每阶段独立可验证、不退化）

<!-- txdoc:07-NET-PLAN-A-V2-PHASES -->

> 原则：**绝不一次性重写**。每阶段结束都能编译、跑 LTP net 不退化、可提交。默认 netns（lo + eth0）跑通核心后，多 netns 才作为独立后续阶段。

### P0 — 解冻：真实时钟 + 常驻 Interface 骨架

> **可执行细化见 [`REFACTOR_P0_v1.md`](REFACTOR_P0_v1.md)**（病根机制 + 四处改动带代码 + 四层测试 + A/B 设计点）。

- 建 `NET_NOW_NS`（D3）；delegate/syscall 入口 store 真实 ns。
- 默认 netns 建**一个常驻 `Interface`**（替一次性 iface）；socket 暂仍现状形态，但 `Context` 来自常驻 iface、`now` 用 `NET_NOW_NS`。
- **修**：①时钟冻结 → 推进；R2a 老化定时器开始有意义。
- **验证**：编译 + loopback TCP/UDP LTP 不退化 + 观测 `now` 推进、smoltcp 重传/老化被 `poll_at` 排上。

### P1 — loopback 恢复 smoltcp 委托 + 瘦 TcpConnection

- loopback `lo` 当设备走手写 poll；loopback RX 走 `socket.process()`、TX 走 `socket.dispatch()`；`connect`/`listen`/accept 走 smoltcp 真握手。
- **删**：`step_tcp_loopback` 直拷、`LoopbackIface`、RawTcpSocket 5 缓冲（先 loopback 路径）。
- **修**：①（loopback 重传）、②（双份/双通路）、③（loopback UDP 旁路）。
- **验证**：loopback TCP/UDP LTP（recv01/connect/tcp_lifecycle 等）、`tests/loopback_tests/*`。

### P2 — 外部网卡走同一手写 poll（打通外部 TCP/UDP/IPv6）

- `eth0`（virtio）走与 loopback 相同手写 poll：RX `ops.receive`→解析→`socket.process(cx,…)`（验校验和）；TX `socket.dispatch`；`socket.connect()` 真发 SYN。
- **修**：①（外部 TCP 结构性不可用 → 可用）、③（外部 UDP）、R3a（RX 验校验和）、⑩（IPv6 数据路径随常驻 Interface + smoltcp process 复活）。
- **验证**：git-over-HTTPS rc=0（对照 `feature-network-next` 已通）、外部 ping/UDP、net_stress、`-smp 4` 并发不卡死。

### P3 — 上半接入：socket 成一等 FileLike + 模型瘦身 + 单锁 + 就绪/等待统一

- **上半接入**：socket 成为一等文件对象——`FileOps` trait 取代 syscall 特判、`step_read/write` 委派 ops（D13/⑤）；socket park 从 legacy `WaitToken`/`yield_now` 迁 `await_wait_source`（D14/⑥/R4a）。
- **模型瘦身**：`SocketPayload` 9×Option → `enum SocketImpl`；StreamSocket `enum State`；每 iface 单锁（D4）；就绪单一来源（D7）。
- **修**：⑤（FileOps 取代特判）、⑥/R4a（wait 收敛 `await_wait_source`、epoll 对 socket 阻塞注册表统一）、⑨（字段）、②（双份/三处记账）、R1b–e（并发）、R2b/c/d/f（close 排空 backlog、所属 ns 表、补 Drop、断引用环）、R2e（conntrack 加界+老化）。
- **验证**：全 LTP net 不退化 + 并发压力 + 内存（大量连接不 OOM、close 不泄漏）。

### P4 — 收尾：分层 + IPv6 + ICMP/DNS feature

- `ether.rs` 拆 `link/`（L2 帧编解码）/ `net/`（L3）；开 `socket-icmp`（ping 走 smoltcp）、按需 `proto-dns`；netfilter/bridge 移出 device 层、桥接确定化（D9）。
- **修**：④（分层）、⑩（IPv6 ping6/路由/邻居）、R3b（分片表 LRU）、§4 device 泄漏、B 裁决的 `Box::leak` 与错误 SAFETY 注释。
- **验证**：IPv6 LTP（ping6/ipv6_lib）、netns/bridge/netfilter LTP、全量 whitelist 对照 main。

### P5（后续阶段，非核心）— 多 netns

- per-netns Interface 集 + veth pair 跨 netns 转发 + bridge + 多 iface poll 调度 + per-netns Drop。难点见 D2。**在 P0–P4 核心稳定后独立立项**，旧 per-netns 代码大概率整段重写。

| 阶段 | 主交付                                 | 修复的审计项             | 验证基线                          |
| ---- | -------------------------------------- | ------------------------ | --------------------------------- |
| P0   | 真实时钟 + 常驻 Interface 骨架         | ①(时钟) R2a             | loopback LTP 不退化               |
| P1   | loopback 恢复 smoltcp 委托 + 瘦 socket | ① ② ③ (loopback)      | loopback TCP/UDP LTP              |
| P2   | 外部网卡走同一 poll                    | ① ③ ⑩ R3a             | git-https/net_stress/-smp4        |
| P3   | socket 成 FileLike + 瘦身 + 单锁 + 等待统一 | ② ⑤ ⑥ ⑨ R1 R2 R4a   | 全 LTP net + 并发 + 内存          |
| P4   | 分层 + IPv6 + ICMP/DNS                 | ④ ⑩ R3b §4泄漏        | IPv6/netns/bridge LTP + 对照 main |
| P5   | 多 netns（后续）                       | netns/veth/bridge 运行时 | netns/bridge LTP                  |

---

## 6. 风险与缓解

<!-- txdoc:07-NET-PLAN-A-V2-RISKS -->

| 风险                             | 说明                                                              | 缓解                                                                           |
| -------------------------------- | ----------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| **触碰核心写路径**         | 改 RX 投递（手写 seq/ack →`process()`）、删 5 缓冲，涉协议主干 | 分阶段 + loopback 先行 + 每阶段对照基线；保留`feature-network-next` 解法参考 |
| **TCG poll 频率**          | 周期 poll 若忙转吃 TCG 预算（net_stress 对 per-packet 成本敏感）  | 严格按聚合`poll_at()` 唤醒（事件驱动）；无事不 poll                          |
| **`&Guard` 贯穿 poll**   | `ops.transmit/receive` 需 guard，手写 poll 期须持 `&'a Guard` | 确认 poll 调用点（delegate step/syscall）guard 生命周期覆盖整轮 poll           |
| **per-netns 生命周期泄漏** | 常驻 Interface + socket ring 大块堆内存                           | D12：补`Drop` 真回收（本身就是 R2 修复项）                                   |
| **LTP 行为兼容**           | 大改动可能引入 net 回归                                           | 分阶段 + 每阶段对照 main/feature-network-next；`tools/` 见证脚本             |
| **继承 smoltcp 固有限制**  | Go-Back-N/无 SACK、无 PMTUD（见 §8）                             | QEMU/LTP 无影响；记为已知限制，真机有损链路再议                                |

---

## 7. 验证策略

<!-- txdoc:07-NET-PLAN-A-V2-VERIFY -->

- **基线**：`main`（无网络改动）+ `feature-network-next`（HTTPS/外部 TCP 已通，对照 P2）。
- **每阶段必过**：`cargo xtask unit`（host）+ 对应 LTP net 子集（QEMU）不退化、loopback TCP/UDP、`-smp 4` 并发不卡死（R1a 哨兵）。
- **P2 关键**：git-over-HTTPS rc=0、外部 UDP/DNS、net_stress。
- **P4 关键**：IPv6（ping6/ipv6_lib）、netns/bridge/netfilter、全量 whitelist 对照 main（沿用既有全量 LTP parity 对照方法）。
- 工具：`tools/` 现有 net 回归/见证脚本（`suite-regression-*`、`ltp-bin-witness` 等）。

---

## 8. 已接受限制 / 暂存（真机阶段再议）

<!-- txdoc:07-NET-PLAN-A-V2-LIMITS -->

这些是 smoltcp 的固有性质，**P0–P4 不处理**，记录在案：

- **Go-Back-N 丢包重传，SACK 解析却忽略**（`tcp.rs:2283-2286`）：丢一包重传整窗。QEMU/loopback/LTP 不丢包 → 无影响；真机有损链路掉吞吐。**用户选择终态要 SACK，但后面再说**——届时需打 fork 补丁让 TCP 读取对端 SACK 区间（脱离 upstream，工作量不小）。
- **无 PMTUD、MTU 单标量、无 ECN/TFO/F-RTO/DSACK**：真机自管 MTU 策略。
- **`max_burst_size` 通告窗口钳制 hack**（`packet.rs:144-161`）：真机若驱动报小 burst 会锁死吞吐 → 驱动缓冲就绪后报 `max_burst_size=None`。
- **`poll()` 一次抽干队列**：真机高负载应改有界收包（`poll_ingress_single`）。
- **IPv6 分片缺失**（`mod.rs:1300-1301`）：与 ⑩ 相关，大 v6 数据报受限。
- **真机网卡驱动**：**本方案范围外**。重构全程瞄准 QEMU/virtio-net；真机以太 MAC 驱动（`NetDeviceOps` 实现）是独立后续工程，与 A0/A1 无关。

---

## 9. 开放问题（讨论清单）

<!-- txdoc:07-NET-PLAN-A-V2-OPEN -->

1. **D9** netfilter/NAT/conntrack 与逐 socket `process` 的协同边界——先保留现有 netfilter 仅做 ingress/egress hook，还是在本方案内一并理顺？
2. 范围确认：netlink(rtnetlink/nfnetlink ~4000 行)、SCTP/RDS/AF_PACKET 这些 smoltcp 不管的协议，P0–P4 **不动**（仅保证与新 engine 共存），是否同意？
3. 是否在 P0 之前先做一个"最小可行原型"（默认 netns、loopback-only、常驻 Interface + 手写 poll + `socket.process`）验证 TCG 性能与 guard 穿线，再全量推进？
4. **P5 多 netns** 的设计何时细化（veth 配对/bridge 转发/poll 调度）——P4 收尾时再开，还是更早？

---

## 附录 A. 证据锚点

<!-- txdoc:07-NET-PLAN-A-V2-EVIDENCE -->

**asterinas-true 模型来源（亲验，`/home/msp/learning/asterinas`）**：

- 自述拒绝 SocketSet/poll 求效率：`kernel/libs/aster-bigtcp/src/lib.rs:3-11`
- 常驻 Interface（一次 new）：`.../iface/poll_iface.rs:18-21`、`.../iface/phy/ether.rs:48`、`.../iface/common.rs:41`
- 自有 SocketTable（非 SocketSet）：`.../iface/common.rs:43`、`.../socket_table.rs`
- 手写 poll + 真实时钟戳 context：`.../iface/common.rs:225-267`、`:242`、`.../iface/time.rs:5-8`
- smoltcp 仅 per-socket：`.../socket/bound/tcp_conn.rs:318/569/616/651`
- socket enum State：`kernel/src/net/socket/ip/stream/mod.rs:57-78`
- loopback 也是 Device：`kernel/src/net/iface/init.rs:62-100`

**smoltcp 限制（亲验，`external/smoltcp-asterinas`）**：

- SocketSet 平铺无索引、O(n) 分发：`src/socket_set.rs:44-46`、`src/iface/interface/tcp.rs:21-30`、`src/iface/interface/mod.rs:447-453/536-549/654-755`
- Go-Back-N/SACK 忽略：`src/socket/tcp.rs:2283-2286`
- 单线程 `&mut self`：`src/iface/interface/mod.rs:433-438`、`src/socket_set.rs:112`
- 校验和 offload 支持：`src/phy/mod.rs:206-214`
- max_burst 窗口钳制：`src/iface/packet.rs:144-161`
- IPv6 分片缺失：`src/iface/interface/mod.rs:1300-1301`

**用户原设计（证实本就正确，`msp/tx-kernel-network-stack-design-v9.md`）**：§9.1 smoltcp 角色边界（不用 poll/SocketSet）、§9.3 薄 TcpConnection、§10.3 EtherIface、§15 PollContext、§16.2 真实时钟 `timer::now_millis()`。

**接入层（上半战线 ⑤⑥）证据锚点**：

- ⑤ 文件接口：socket 嵌入统一 VFS `vfs/structure.rs:560-563`；`step_read/write` 对 socket 主动 `EINVAL` `vfs/execution.rs:396-399/649-652`；CharDevice ops 多态对照 `vfs/execution.rs:382/636`；syscall 特判全表 `io.rs:1946/2229/1100/1497`、`fs_basic.rs:1516/1199`、`epoll.rs:119/188/255`、`splice.rs:69`。
- ⑥ wait 接入：`net/execution/mod.rs:97-131`（构造 WaitToken）、`socket/helpers.rs:1703-1707`（`wait_on_yield_shape` 内 `WaitToken::new`）、`socket.rs:877/883/892/1078`（`yield_now` 忙让步）、`wait.rs:1-6`（注释自承 legacy）、`readiness.rs:30-34`（`RawQueue` 载体本就统一）。已迁 `await_wait_source` 的 8 子系统：eventfd/timerfd/signalfd/epoll/aio/userfaultfd/ipc/proc。

**当前偏离（`feature-network-refactor @ fd64ba24`）**：`tcp.rs:712-720`（一次性 Interface）、`tcp.rs:24-33`（5 缓冲）、`step_send.rs:363-426`（直拷）、`payload.rs:33-53`（9 Option）、`ether.rs:121-133`（L2/L3/ICMP 融合）。

---

*本方案两条战线：下半·引擎四主轴已定（D1 asterinas-true / D2 单 netns 先行 / D3 NET_NOW_NS / D4 每 iface 单锁）；上半·接入方向已定（D13 FileOps ⑤ / D14 统一 wait ⑥）。定稿后据此拆 P0 执行计划。所有 `file:line` 按 `fd64ba24`，asterinas 锚点按其当时 HEAD。*
