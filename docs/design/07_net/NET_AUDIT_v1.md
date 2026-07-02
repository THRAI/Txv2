# 网络栈现状调研（重构前审计）

<!-- txdoc:07-NET-AUDIT-V1 -->

**Status.** v1 (2026-06-29)。重构前**现状审计**，非设计规范。本文描述代码*当前是什么样*与*为什么是问题*，不规定*应该重写成什么*（重构方向只给到"主线指向"，详细新设计另立文档）。

**Purpose.** 在动手重构网络栈之前，把使用者提出的 9 条怀疑逐条取证、裁定（证实／部分／证伪），用 `file:line` 钉死证据，并对比 Linux／smoltcp 的做法，给出代价与重构主线。**核心约束：不无中生有——每条结论可复核，证伪用户的怀疑时同样给反证。**

**Audience.** 网络栈重构作者、审阅者；以及任何想知道"为什么这栈有 4 万行、TCP 为何只在 loopback 能跑"的人。

**Targets.** qemu-riscv64-virt, qemu-loongarch64（及后续真实板）。

**调研方法.** 5 个并行取证调查员（按正交维度分工：数据路径／socket 模型／分层／内核集成／代码规模）下钻 `crates/tx-subsystems/src/net/`（41080 行），orchestrator 对每个维度最关键／最反直觉的论断**亲自复核行号**。5 维度抽查命中率极高，仅发现 2 处调查员数字小误差（已用实测值修正，见 §6）。

**⚠️ 重要背景（影响重构策略）.** 当前分支 `feature-network-refactor`，HEAD = `fd64ba24`。会话上下文中两个 "net/vfs: route socket … through unified step path (Phase 0/1)" 提交（`10a878d7`/`fd09957b`）**不在当前 HEAD 的祖先链**（`git merge-base --is-ancestor` 实测为否），它们只活在 `feature-network-next`。同理，先前为 git-over-HTTPS 做的"真实 smoltcp 时钟 / feed-ACK-to-smoltcp"修复也在 `feature-network-next`，**未入 main**。**因此本审计描述的是 main 的真实状态；使用者基于 main 重构所看到的问题全部成立。** `feature-network-next` 有部分问题的现成解法可借鉴（已在相关章节标注）。

**可信度图例.**
- 🔬 **亲验** — orchestrator 亲自 `Read`/`grep` 复核了行号。
- 📋 **实测** — 调查员取证；其可靠性已被抽样核验（命中率高）。
- 💭 **推断** — 由上述事实推导的后果，未经运行时复现，按推断标注。

**Companion documents.**
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — 四模块子系统布局与五阶段 step 纪律（重构目标形态的参照）。
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — `RawQueue`/`RawPort` 发布原语（net 的等待机制就建在其上，见 §5）。
- [`DEVICE.md`](../06_devices/DEVICE.md) — 设备子系统与 VFS 四条数据路径（"`i_fops` 去哪了"——本审计 §5 给出 net 侧答案）。
- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) — `Cap`/`PayloadCap`/identity 分解（net core 在此点上其实较健康，见 §5）。

---

## 0. 核心结论

<!-- txdoc:07-NET-AUDIT-VERDICT -->

**一句话病根.** 网络栈把 smoltcp **降格**为"线协议（wire）编解码库 + 仅 loopback 用到的单连接状态机片段"，并在其上**自研了一整套并行的 socket 缓冲／状态／iface／路由**。两套机制（smoltcp 与自研层）**并存且互不信任**：loopback 路径整体**旁路** smoltcp（直接内存直拷 + 手工置 `Connected` + 跳过三次握手）；冻结的时钟（根本没有持久的 smoltcp `Interface`）让 smoltcp 的全部定时机制（重传／老化／TIME_WAIT）失效，导致**面向物理网卡的外部 TCP 在当前分支结构性不可用**；这套自研层又通过 **syscall 层的 `if 是 socket` 特判**（而非统一的 file-ops 接口）接入内核。代码量（4 万行）本身**不是病**——它来自协议广度，不是垃圾。

**使用者 9 条怀疑——裁定速览.**

| # | 怀疑 | 裁定 | 一句话 |
|---|---|---|---|
| ① | TCP 不完整：时钟冻结／无真重传／Tx-Rx 不对称／Rx 不经 smoltcp | ✅ **证实**（根因） | 无持久 Interface、时钟恒 `Instant::ZERO`；外部 TCP RX 旁路且丢 seq/ack；外部握手缺失 |
| ② | RawTcpSocket 与 smoltcp 职责不清、字段随意、`last_syn_ack` 冲突 | ✅ **证实** | 双数据通路、5 份缓冲、TCP 状态三处记账 |
| ③ | UDP 与 TCP 情况类似 | ◐ **部分** | UDP 也整体旁路 smoltcp，但因无握手/定时器，冻结时钟无害 → 功能上可用 |
| ④ | 网络层与链路层融合成一个文件 | ✅ **证实** | `ether.rs` 单文件单结构体融合 L2+L3+ICMP+设备 TX |
| ⑤ | 无文件系统接口，全硬编码特判转移 | ◐ **分发层证实／存储层证伪** | socket 确在 VFS 结构内，但通用 fd 操作全靠 `if 是 socket` 特判，统一读写路径主动拒绝 socket |
| ⑥ | `wait_shim`（net 等待接入）与内核其他子系统不一样 | ◐ **部分证实** | 底层 `WaitSource` 注册表统一；但 net/socket 接入停在 legacy `WaitToken`+`yield_now` 忙让步，其余 8 个 fd 子系统已迁 `await_wait_source` |
| ⑦ | 4 万行 = 大量无用测试/死代码 | ✗ **证伪** | 测试健康（0 ignore）、死代码近零；膨胀来自协议广度 |
| ⑧ | 与其它子系统基本无关、硬编码 | ◐ **部分证伪** | net core 边界其实较健康；真耦合点在 delegate↔reactor、syscall seam、缓冲内存 |
| ⑨ | TCP 字段不集中、SocketPayload 没设计好 | ✅ **证实** | 9 个 `Option<RawXSocket>` 平铺、TCP 字段散落三处 |
| ⑩ | IPv6 协议实现不完全（使用者补充） | ✅ **证实（严重）** | 数据路径几乎全断：外部 RX `Unsupported`、无以太 v6 TX、loopback 0 v6、无 v6 路由、NDISC 只写不学、ICMPv6 只收不发；仅控制面/类型层有"形" |

**对你最关心两点的直接回应.**
- *"重传根本没实现，只是同步通过"*——**确实如此，但根因比你想的更深**：不是漏调 smoltcp 的重传，而是（a）每次 TCP 操作都临时 `new` 一个零时刻 Interface 用完即弃、根本没有持久状态来跑定时器；（b）能跑通的 loopback 是**同步内存直拷**，可靠传输不暴露重传需求。
- *"不知道为什么有 4 万行，是不是无用测试"*——**不是**。测试占 ~34% 且健康，死代码近零。4 万行来自自研 ~10 种协议（TCP/UDP/ICMP/raw/rtnetlink/netfilter/unix/RDS/SCTP/AF_PACKET）+ netlink wire 样板。**真正的债是设计耦合（双份机制、巨型 struct、分层崩塌），不是行数。**

---

## 1. 根因：smoltcp 被降格 + 时钟冻结（怀疑①）

<!-- txdoc:07-NET-AUDIT-CLOCK -->

### 1.1 没有持久 Interface，时钟永久冻结在 `Instant::ZERO`

**裁定：✅ 证实。** 这是整栈的致命根因。

- 🔬 **无持久 Interface**：`protocol/tcp.rs:712-720` 的 `with_context` 每次被调用都临时 `new` 一个 `Loopback` device + `Interface(…, Instant::ZERO)`，跑完闭包**即丢弃**。`connect`/`dispatch_segment`/`process_segment`（tcp.rs:408-486）全经它。全生产代码 `Interface::new` **仅此一处**（其余在 tests）。
  ```rust
  fn with_context<R>(f: impl FnOnce(&mut smoltcp::iface::Context) -> R) -> R {
      let mut device = Loopback::new(Medium::Ip);
      let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut device,
                                     smoltcp::time::Instant::ZERO);   // 时间恒 0
      f(iface.context())   // 用完即弃
  }
  ```
- 🔬 **生产代码 15 处硬编码 `Instant::ZERO`**：`execution/step_tcp_loopback.rs:190,319`、`step_udp_loopback.rs:54,76`、`step_icmp_loopback.rs:46`、`step_device_tx.rs:92`、`step_process_network_events.rs:56`、`namespace.rs:1778`、`step_loopback_pending.rs:255` 等。
- 🔬 **内核有真实时钟，但 net 栈零接入**：`tx-subsystems/src/wall_clock.rs:128` 提供 `monotonic_now_ns::<TimeIf>()`；`grep monotonic_now_ns|wall_clock|TimeIf` 在 net 生产代码为空。
- 📋 **delegate 层确有真实 `now()`**：`tx-kernel/src/init/net.rs:227-230`（`read_ns()/1000`），经 `runtime.rs:203-208` 喂给各 step——**但只用于 ARP/ICMP 计时与 TCP backlog 截止，从不流入 smoltcp socket 运算**。
- 🔬 **唯一非零时间戳是个 workaround**：`protocol/ether.rs:392,408` 把 ARP 表项过期设为 `Instant::from_secs(10*365*…)`（10 年≈永不过期）——正是时钟冻结逼出来的（无法正常老化）。

**Linux 对比.** Linux 用真实推进的 `jiffies`/`tcp_time_stamp` 驱动 RTO、delayed-ACK、keepalive、TIME_WAIT、PAWS。时钟是这些机制的发动机。

**代价（💭推断）.** smoltcp 内一切依赖时间前进的行为（重传定时器、延迟 ACK、keepalive、连接超时、邻居老化）在 `now≡0` 下永不触发或逻辑失效。

**重构主线.** 建立 **per-iface 持久 smoltcp `Interface`**，由 reactor 以 `P::read_ns()` 真实时间周期 `poll()`；废弃 `with_context` 一次性 Interface。（`feature-network-next` 的 `NET_NOW_MICROS` 是此方向的雏形，可借鉴。）

### 1.2 没有通用 TCP 重传

**裁定：✅ 证实（"无真重传"成立）。**

- 📋 全栈无 `poll_at`/`poll_delay`/`rto` 的生产使用（从不向 smoltcp 询问"下次何时重传"）。
- 📋 delegate 的 `next_deadline` **只来自 backlog**（`delegate/runtime.rs:216-217,296`；deadline 全来自 listener 的 `tcp_backlog_next_deadline()`），无任何 smoltcp socket 级 deadline 入账。
- 📋 唯一的"重传"动作：`execution/step_tcp_backlog_poll.rs:51-70` 把存好的 `last_syn_ack` 重投 **loopback 队列**——这是 accept 半开队列保活，**不是数据重传**，且仅 loopback。
- 💭 由 §1.1：`with_context` 恒 `Instant::ZERO`，smoltcp 重传 deadline = `ZERO+rto`，而后续 `now` 仍是 `ZERO < ZERO+rto` → 永不触发（基于 smoltcp 语义推断，未运行时复现）。

### 1.3 Tx/Rx 严重不对称，外部 Rx 完全绕过 smoltcp

**裁定：✅ 证实（"Tx-Rx 不对称"+"Rx 不经 smoltcp"成立，且分场景）。**

- 📋 **外部 TX（经 smoltcp）**：`step_send`→入 smoltcp send buffer（`tcp.rs:256,510-512`）→ delegate `process_tcp_tx_socket`（`step_device_tx.rs:191-216`）→ `raw_tcp.dispatch_segment()`（smoltcp 生成带真实 seq/ack/window 的段）→ `dispatch_ip_at` → 网卡。
- 📋 **外部 RX（绕过 smoltcp）**：`process_frame_at`（手工解析）→ `demux_tcp`（`packet/smoltcp_demux.rs:41-60`）→ **`TcpPacketFlags` 只有 `syn/ack/rst`，seq/ack 号被丢弃**（`packet/demux.rs:31-36`）→ `process_tcp_event`（`step_process_network_events.rs:275-324`）→ `record_recv_payload` 直接塞 RX 缓冲 + `record_send_space(max(1,len))` **假 ACK 记账**。`process_segment`（smoltcp 的 `accepts/process`）**不被外部路径调用**。
- 💭 **本质**：TX 侧 smoltcp 以为发了带序号的数据并等 ACK；RX 侧 ACK 永不回喂 smoltcp ⇒ smoltcp 发送窗口/未确认队列永不推进；发够一个初始窗口后即停发且不重传。单连接被劈成**两个互不通信的状态视图**。

### 1.4 外部 TCP 主动连接/被动 accept 结构性缺失

**裁定：✅ 证实。** （这解释了为什么"连基本的 TCP 都不完整"。）

- 📋 smoltcp `connect_endpoint`（→`socket.connect`，`tcp.rs:403-417`）**唯一生产调用点是 loopback**（`step_tcp_loopback.rs:316`）。
- 📋 `step_connect`（`execution/step_connect.rs:58-118`）对外部目标只把状态置 `Connecting` 后 `yield`，**从不调 smoltcp connect**（即外部 open 不会发 SYN）。
- 📋 入站 SYN-ACK（`syn && ack`）在 `process_tcp_event` 两个分支都不匹配 ⇒ **被丢弃**。
- 📋 **TCP 真正能跑的只有"同步直拷"**：`tcp_uses_direct_stream`（`step_send.rs:363-370`）让 `has_connected==false` 的 TCP 走 `send_tcp_stream_bytes`（389-426），直接把字节拷进对端 socket 的 RX 缓冲（`payload.rs:634-639`）+ `fire_recv`。对真实外部对端，`lookup_tcp_connected_peer` 找不到本地 peer ⇒ `EPIPE`。
- ⚠️ 调查员独立指出："外部 TCP 结构性不可用"与记忆中 "2026-06-22 git clone https rc=0" 不符，疑回归——**已查明真相**：那批修复在 `feature-network-next`，未入 main（见文首背景）。

**Linux 对比.** Linux 所有入站段都进 `tcp_v4_rcv`，由内核生成 ACK/SYN-ACK/RST；TX/RX 共用同一 `tcp_sock`，RX 的 ACK 驱动 TX 窗口与 RTT。

**重构主线.** per-iface 持久 Interface + 真实时钟周期 `poll()`，让**收、发、握手、重传统一回到 smoltcp 状态机**；外部与 loopback 走同一条路径。

---

## 2. Socket 对象模型与字段失控（怀疑②⑨）

<!-- txdoc:07-NET-AUDIT-SOCKET -->

### 2.1 双数据通路：同一类型两种互斥语义

**裁定：✅ 证实。**

- 🔬 `RawTcpSocket`（`protocol/tcp.rs:24-33`）**同时**持有 smoltcp `tcp::Socket`（内含自己的 rx/tx buf）**和**一套自维护字节队列。
- 🔬 `tcp_uses_direct_stream`（`step_send.rs:363-370`）用"高层 `TcpState::Connected` 但 smoltcp `has_connected==false`"这种**状态不一致**来切换路径：命中则走 loopback 直拷（不碰 smoltcp），否则走 smoltcp。
- 📋 loopback connect 直接置 `TcpState::Connected`、**跳过三次握手**（`step_connect.rs:196-201`）。

### 2.2 一个 TCP socket 五份缓冲

**裁定：✅ 证实（"rx_buffer/tx_buffer 作用"——实为"重复"而非"虚设"）。**

- 🔬 `RawTcpSocket`（tcp.rs:24-33）字段：`socket`(smoltcp，内含 rx_buf+tx_buf) + `rx_buffer` + `tx_buffer`（staging）+ `corked_tx` = **5 个缓冲**。
- 📋 TX 双写：`enqueue` 既 `send_slice` 进 smoltcp 又压 staging（tcp.rs:256,263-265）；RX 双存：smoltcp 路径 `recv_slice` 拷进 staging（tcp.rs:488-508）。双重流控需手工同步（`send_available = min(staged, protocol)`，tcp.rs:185-200）。
- 📋 smoltcp socket 创建即预分配 `recv+send`（TCP 默认 328KB，tcp.rs:542-543），而 loopback 连接根本不用它 → 💭 单 socket 内存近乎翻倍 + 纯浪费。
- 📋 **反证（防夸大）**：`rx_buffer`/`tx_buffer` 不是死字段——它们是 loopback 路径的主存储、并承载 smoltcp 路径流控。问题在"重复"，不在"未用"。

### 2.3 TCP 状态三处记账 + `last_syn_ack` 越权

**裁定：✅ 证实（"`last_syn_ack` 与状态机职责冲突"成立）。**

- 🔬/📋 三处并行记账可不一致：① 高层 `SocketProtocol::Tcp(TcpState)`（`structure/types.rs:949-967`）；② `RawTcpProtocolState{has_connected,is_recv_shut,is_rst_closed}`（tcp.rs:36-40）；③ smoltcp `tcp::Socket::state()`。`tcp_uses_direct_stream` 正是**靠这种不一致**区分路径。
- 📋 `last_syn_ack`（tcp.rs:27）手工缓存最后一个 SYN-ACK 段（写 `remember_syn_ack` tcp.rs:530-534，读 `retransmit_syn_ack_segment` tcp.rs:444-446），由自研 backlog 计时器驱动重发——这正是 smoltcp 在 `SynReceived` 下**本该自动做**的事。

### 2.4 SocketPayload：巨型 struct 塞所有协议（怀疑⑨核心）

**裁定：✅ 证实。**

- 🔬 `SocketPayload`（`structure/payload.rs:33-53`）平铺 **9 个 `Option<RawXSocket>`**（`raw_tcp/udp/icmp/unix/rds/sctp/packet/netlink_route/netlink_netfilter`，行 39-47）——互斥（任一时刻至多一个 `Some`），却全部 inline 占空间。
- 📋 所有按类型分派的代码被迫写成八/九元组巨型 `match`（如 `raw_recv_available` payload.rs:1214-1239、`consume_recv_bytes_into` payload.rs:688-814）。💭 这是 `enum` 误用为 `struct` 的教科书案例。
- 📋 **TCP 字段散落三处**（`protocol` / `raw_tcp` / `tcp_backlog`，行 36/39/50），各自加锁，要读全 TCP 状态需同时持三把锁。
- 📋 `IpEndpoint`（types.rs:255-261）**同载 v4+v6 地址**，靠 `family` 字段约定，类型层面无法阻止"family=Inet 却读 addr6"。
- 📋 `SocketOptionSet`（types.rs:633-639）无条件全携带 `socket+ip+tcp+sctp` 四组——一个 UDP socket 也带着 28 字段的 `SctpLevelOptions`（types.rs:585-619），`default_udp()` 也要填整套 SCTP 默认值。
- 📋 `SocketProtocol::Sctp(TcpState)`（payload.rs:2323）复用 TCP 状态枚举，语义错配。
- 📋 **现成的干净样板**：`RawPacketSocket`/`RawRdsSocket`/`RawSctpSocket`（payload.rs:1392/1499/1642）字段完全一致（`state + recv_limit + send_space`），无 smoltcp 双份、无散落——证明栈内**已具备**一套自洽 socket 形态，TCP/UDP 的双份是历史包袱。

**Linux 对比.** `struct sock` 公共头 + 协议私有结构（`tcp_sock`/`udp_sock`）经 `sk_prot` 多态的**继承式布局**：互斥协议不共占空间，选项分层存，`sockaddr_in`/`in6` 是独立类型。

**重构主线.** ① 9 个 `Option` → 单一 `enum SocketImpl{Tcp(..),Udp(..),…}`，消灭八元组 match；② TCP 三处字段内聚进 TCP variant；③ 缓冲**单一归属**（统一走 smoltcp 删 staging，或彻底自研删 smoltcp socket、对齐 RawRds 形态）；④ `IpEndpoint→enum{V4,V6}` 让类型携带不变量；⑤ `SocketOptionSet` 按协议分层；⑥ 删 `last_syn_ack`，重传交 smoltcp。

---

## 3. UDP 与 ICMP（怀疑③）

<!-- txdoc:07-NET-AUDIT-UDP-ICMP -->

**裁定：◐ 部分证实。** UDP **确实**和 TCP 一样整体旁路 smoltcp，但因 UDP 无握手、无定时器，冻结时钟**无副作用** → 功能上可用（这解释了 DNS 能解析）。

- 📋 **UDP smoltcp socket 是死重**：`RawUdpSocket`（`protocol/udp.rs:20-29`）持有 smoltcp `udp::Socket`，但收发全走自维护 `rx_datagrams`/`tx_datagrams`（udp.rs:99-113,254-261）；smoltcp 子对象仅 `close` 在生产用到（`step_socket_close.rs:154`），`can_recv`/`can_send` 仅测试用。
- 📋 **ICMP 反而最干净**：`RawIcmpSocket`（`protocol/icmp.rs:78-84`）**无 smoltcp socket**，纯自研；但 v4/v6 收队列类型不对称（`rx_queue` 存解析后的 `Icmpv4EchoPacket` vs `rx_ipv6_queue` 存原始 `RawIpv6Packet`），且 TX 仅 v4。
- 📋 **ICMP 回包手工合成（不经 smoltcp）**：外部 `EtherIface::maybe_reply_icmpv4`（`ether.rs:586-608`）在收帧链里直接合成 echo reply 并 `dispatch_ip_at` 同步发回；loopback `poll_icmp_ingress`（`poll_context.rs:360-415`）同理。

**重构主线.** UDP 删 smoltcp 子对象字段，对齐 RawRds 的 `state+recv_limit+send_space`（smoltcp 的 UDP 编解码可保留为无状态函数）；ICMP 统一 v4/v6 收队列抽象层级、补 v6 TX；三者收发统一回 iface poll/路由层，避免 TCP 复活后又两套并存。

---

## 3-bis. IPv6 协议栈残缺（怀疑⑩ · 使用者补充）

<!-- txdoc:07-NET-AUDIT-IPV6 -->

**裁定：✅ 证实（严重）。** IPv6 是"有壳无数据路径"——地址/类型/配置/管理面齐备，但收发/路由/邻居学习/loopback 几乎全断。

**逐层对比（全部 🔬 grep/亲验）：**

| 层 | IPv4 | IPv6 | 证据 |
|---|---|---|---|
| 外部 RX（网卡收） | 解析 + 重组 | **直接 `Unsupported`** | `protocol/ether.rs:279`（`Ipv6 => PacketDispatch::Unsupported`） |
| 外部 TX（网卡发） | `dispatch_ip_at` / `transmit_ipv4_packet` | **无任何 `dispatch_ipv6`/`transmit_ipv6`** | `protocol/ether.rs`（grep 空） |
| loopback | 完整（同步直拷） | **0 处 v6 代码** | `protocol/loopback.rs`（grep 空） |
| 路由 / 转发 | `best_ipv4_route` / `add_ipv4_route` | **无 v6 路由函数** | `namespace.rs`（grep 空；`best_ipv4_route` 是 v4 专名） |
| 邻居解析 | ARP 动态学习（`process_arp`/`learn_arp`/`resolve_or_request`） | NDISC **只写不学**（`install/remove_static_ndisc`/`ndisc_snapshot`/merge，**无 RX learn 路径**） | `protocol/ether.rs:403-486` |
| ICMP echo | 收 + 发 | 能收（`rx_ipv6_queue`），**发不出**（`tx_queue: VecDeque<Icmpv4EchoPacket>`，`enqueue_tx_echo` 只收 v4） | `protocol/icmp.rs:80-81,151` |

**有"形"的部分（控制面/类型层，确实存在，避免一概抹杀）：** 地址类型 `Ipv6Address`/`IpEndpoint.addr6`/`AddressFamily::Inet6`；`to_smoltcp_endpoint` 支持 `Inet6`（`protocol/tcp.rs:723-725`）；`namespace.rs`（98 处 v6）/`rtnetlink.rs`（26 处）的 v6 字段 + procfs `if_inet6`；`Icmpv6EchoPacket`/`RawIpv6Packet` 结构（`protocol/icmp.rs:33-39,71-73`）。

**结论：** 当前 main 上 **IPv6 几乎无法实际收发**——连 IPv4 能跑的 loopback 同步直拷，v6 都没有。可用的仅"接收解析 ICMPv6 入队 + 静态 NDISC 配置 + 管理面展示"。

**与根因的关系（重要）：** 因为 §1/§4 的根因——"没有统一 Interface + 把 smoltcp 仅当 wire 库"——IPv6 等于要在自研层把 IPv4 的每条路径（RX 分用 / TX dispatch / 路由 / 邻居 / loopback）**再实现一遍**，目前只零碎实现了几块。**这是重构方案 A（持久 smoltcp Interface）的又一有力理由**：smoltcp 的 `iface::Interface` 本身就统一处理 v4/v6 的邻居发现/路由，走 A 则 IPv6 **顺带复活**，无需手工补第二套。

> ⚠️ `feature-network-next` 上有过 IPv6 procfs glue + ping6/tracepath6 的工作，同样**未入 main**；且即便那条分支，无 v6 路由 / 无以太 v6 TX 的数据路径短板大概率仍在（建议届时复核）。

**重构主线.** 不单独补 v6（会重蹈"自研第二套"覆辙）；随 §1 的持久 `Interface` + §4 的统一 `trait Interface` 一并，让 smoltcp 统一承载 v4/v6 的收发/邻居/路由。

---

## 4. 分层边界崩塌（怀疑④）

<!-- txdoc:07-NET-AUDIT-LAYERING -->

### 4.1 `ether.rs` 单文件单结构体融合 L2+L3+ICMP+设备TX

**裁定：✅ 证实。**

- 🔬 `EtherIface`（`protocol/ether.rs:121-133`）一个 struct 同时持有：device 句柄（122）、L3 接口配置 `common`（123）、L2 地址（124）、ARP 表（125-126）、IPv6 NDISC 表（127）、IPv4 分片重组表（128-129）。
- 🔬 `process_frame_at`（ether.rs:229-281）一次调用穿透 L2/L3/L4：解以太帧（240）→ 目的过滤（248）→ IPv4 重组（255）→ **L3/L4 ICMP 同步应答**（275）。`dispatch_ip_at`（283-323）一个函数走完 L3 路由（300）→ L2 ARP（311）→ L3 分片/L2 发送（322）。
- 🔬 **RX 重复编解码**（最反直觉、已亲验）：入站 IPv4 帧被 `EthernetFrame::new_checked` 解析（240）→ `prepare_ipv4_ingress` 解 IPv4 做重组（255）→ **`build_ipv4_ethernet_frame` 重新封装成合成以太帧**（263-268）→ `demux_rx_frame_with_smoltcp` 又解一遍以太+IPv4（274，`smoltcp_demux.rs:10,24`）。即每帧以太解析 ×2、IPv4 解析 ×2 + 一次无谓重封装。

### 4.2 无统一 Interface 抽象，两套平行栈

**裁定：✅ 证实。**

- 📋 全模块唯一 trait 是 `NetDeviceOps`（`device.rs:73`）；**没有 `Interface` trait**。`EtherIface`（ether.rs:121）与 `LoopbackIface`（loopback.rs:19）是两个独立 struct、两套独立收发路径；调度层按具体类型分支（`delegate/runtime.rs:37,48` 两个独立 hook）。
- 📋 **路由分裂**：FIB（`namespace.rs:977-983` `best_ipv4_route`，含网关/oif）vs per-iface（`ether.rs:1218` `decide_ipv4_route`，只看本 iface 子网）；转发时 `dispatch_ip_at` **不接收 next_hop**、内部重算 → 💭 FIB 算出的网关被丢弃，非直连网关路由不可靠（基于签名推断，未实机复现）。
- 📋 **冗余抽象**：`SmoltcpAdapter`/`EtherPacketSource` 仅 tests 构造，生产未用。

### 4.3 设备层泄漏

**裁定：◐ 部分证实**（主体干净，三处泄漏）。

- 📋 **反证（防夸大）**：`veth`/`virtio`/`dummy` 作为 `NetDeviceOps` 实现是干净的（只做帧收发），`veth.rs:4-6` 注释甚至正确陈述了边界。
- 📋 泄漏①：`device/bridge.rs:11-13,210,…` 直接调 netfilter（L2 设备内嵌包过滤）。泄漏②：通用 `NetDeviceOps` trait 内置 `bridge_*` 方法（`device.rs:86-108`），污染每个设备契约。泄漏③：`device/virtio.rs:7-8,333,362` 反向依赖 `delegate` 调度层。
- 📋 跨层放置：L2 地址 `EthernetAddress` 定义在 `device.rs:40-61`，被协议层向上越界依赖。

**Linux/smoltcp 对比.** Linux 每层独立编译单元（`eth.c`/`arp.c`+`neighbour.c`/`route.c`/`ip_fragment.c`/`icmp.c`），ICMP 应答在 `ip_local_deliver` 之后、绝不在 L2；bridge 是独立子系统、`br_netfilter` 可选。smoltcp 自身用单一 `iface::Interface` 统管 neighbor/ARP/IP/路由。txKernel 两者都没对齐：自研三套 iface + 把 smoltcp 当 wire 库。

**重构主线.** `ether.rs` 拆为 `link/ethernet`（仅帧编解码）+ `l2/neigh`（ARP+NDISC，协议无关）+ `l3/ipv4_route` + `l3/ipv4_fragment` + `l3/icmp_input`；引入统一 `trait Interface`，`EtherIface`/`LoopbackIface` 实现它、调度多态；路由统一 FIB（查找产出 `{oif,next_hop}`，`dispatch_ip_at` 收显式 `next_hop`）；ICMP 应答移到 L3 收包路径；`bridge_*` 移出 `NetDeviceOps`，`EthernetAddress` 下沉 wire 基础模块。

---

## 5. 与内核 / VFS 的集成（怀疑⑤⑥⑧）

<!-- txdoc:07-NET-AUDIT-VFS -->

### 5.1 怀疑⑤ 文件接口：◐ 分发层证实 / 存储层证伪

- 📋 **存储层（证伪"完全游离"）**：socket 确实嵌在统一 VFS 结构内 —— `OpenFile → RNode → StructPayload::Socket{ identity: Cap<SocketIdentity> }`（`vfs/structure.rs:560-563`）。它**不是**脱离 VFS 的孤儿。
- 📋 **分发层（证实"硬编码特判转移"）**：**无 `file_operations` 式多态**。统一的 `OpenFile::step_read/step_write` **主动对 socket 返回 `EINVAL`**（`vfs/execution.rs:396-399,649-652`），把 socket I/O 逼到平行的 `sendto/recvfrom`。通用 fd 操作全靠 syscall 层特判：

  | syscall | 位置 | 行为 |
  |---|---|---|
  | write | `io.rs:1946-1948` | `if 是 socket` → `sys_sendto` |
  | read | `io.rs:2229-2245` | `if 是 socket` → `sys_recvfrom` |
  | ioctl | `fs_basic.rs:1516-1520` | `StructPayload::Socket => sys_socket_ioctl` |
  | ppoll/pselect6 | `io.rs:1100-1144 / 1497-1530` | `socket_poll_mask/_wait_token_from_file` |
  | epoll | `epoll.rs:119,188,255` | 同上 |
  | splice | `splice.rs:69-88` | `is socket` → 拒绝 |
  | close | `fs_basic.rs:1199` | 通用 close 后调 socket 善后钩子 |

- 📋 **关键对照**：框架**本有多态能力**——`StructPayload::CharDevice(binding) => binding.ops.read(...)`（`vfs/execution.rs:382,636`）。字符设备走 trait 式 ops，socket 本可同样挂一组 ops，却在该位置选择返回 `EINVAL`。
- ℹ️ `feature-network-next` 的 Phase 0/1 是朝这个方向走的半步（把 socket arm 从 EINVAL 改为转发、把特判从"全 socket"收窄到"仅 netlink"），但**不在当前分支**，且仍是 enum-arm 转发而非 trait 多态。

### 5.2 怀疑⑥ wait_shim：◐ 部分证实（按使用者原意改判）

> **裁定修订记录.** 本文 v1 初稿把本条理解为"net 有一套*私有*等待原语"并判 ✗证伪。使用者澄清原意是"**net 用的等待接入与内核其他子系统不一样**"——按此原意复查后改判 **◐ 部分证实**：原证伪只对"私有原语"那个命题成立，对使用者真正的命题（接入形态不一致）则**成立**。

- 📋 **底层原语统一（原判仍成立的部分）**：`wait_shim` 这个名字不存在；net 的等待载体是 substrate `RawQueue`（`net/structure/readiness.rs:30-34`，由 `bus_readiness!` 宏生成）+ 统一 `WaitSource` 注册表（`tx-substrate/src/wake/wait_source.rs`），与 pipe/tty/futex/vfs 同源（`v3_*_waitsource.rs` 测试为证）。**这一层 net 没有自己另起炉灶。**
- 🔬 **但接入适配是两套，net/socket 用旧的那套（使用者原意成立）**：
  - **新形态** `await_wait_source(ctx,…).await`（`tx-shims/src/linux_syscall/wait.rs`）——已迁移的 **8 个** fd 子系统：`eventfd / timerfd / signalfd / epoll / aio / userfaultfd / ipc / proc`。
  - **legacy 形态（net/socket 仍用）**：`net/execution/mod.rs:97-131` 全程构造 `WaitToken`（`socket_recv/send/accept_wait_token`）；socket syscall 经 `wait_on_yield_shape`（`socket/helpers.rs:1703-1707`，**内部仍 `WaitToken::new(...)`**）；并大量用 `tx_reactor::yield_now().await` **忙让步**（`socket.rs:877,883,892,1078`）——非真正挂起等唤醒，而是反复让出再重试。
  - `wait.rs:1-6` 注释**自承** `WaitToken` 为 **legacy**，并指明"仍 hand-drive 子系统 step 的阻塞 arm 应收敛到 `await_wait_source`、而非构造 legacy `WaitToken`"。net 的 socket step 正是 hand-drive step，却**唯独没收敛过来**。
- **小结**：**底层统一、接入分裂**；net/socket 是全栈 fd 子系统里唯一停在 legacy `WaitToken` + `yield_now` 忙让步的，且比 `await_wait_source` 更原始。

**Linux 对比.** Linux 全 fd 类型经统一 `wait_queue` + `wake_up`，无"部分子系统用新 park、部分用旧 token + 忙让步"的代际分裂。

**重构主线.** 把 socket 的 park 从 `WaitToken`/`wait_on_yield_shape`/`yield_now` **收敛到 `await_wait_source`**，与 eventfd/epoll 对齐；删除 net 侧 legacy `WaitToken` 构造。

### 5.3 怀疑⑧ 子系统耦合：◐ 部分证伪

- 📋 **net core 边界其实较健康（证伪"硬编码 reach-in"）**：net core **不碰 fd 表**（`net/` 内 `grep fd_table|resolve_fd` 为空，fd 逻辑全在 shim 侧）；对进程只以**角色形 `Cap<ProcessIdentity>` 参数**传入（`net/project.rs:394`），无 `current_task` 全局；`SocketIdentity` 是 substrate zone 的 `Cap`；唤醒走统一 bus。这些**符合** CLAUDE.md 的角色形接口要求，重构应**保留**。
- 📋 **真正的耦合点**：① net delegate 直接 import reactor 具体类型 `tx_reactor::wait::{Channel,Mask,…}`（`delegate/timer.rs:2`、`runtime.rs:2` 等），跨 crate 编译期耦合；② `SyscallCtx` 用裸 `Arc<TaskMailbox>`/`Arc<DelegateRegistry>` 串联（`ctx.rs:24,29`）；③ packet/device 缓冲用裸 `Box`/`Vec`，未走 substrate page/reservation 记账。

**Linux 对比.** Linux fd → `struct file`，`f_op` 虚表多态；`vfs_read/vfs_write/sock_ioctl` 一律经 `f_op->...`，**VFS 永不判"是不是 socket"**；socket 由 `socket_file_ops`（`net/socket.c`）提供实现。

**重构主线.** 给 `StructPayload`（或 `OpenFile`）引入与 `CharDeviceBinding.ops` 同形的 `FileOps` trait（read/write/poll/ioctl/close），socket 实现它，**删除 io.rs/fs_basic.rs/epoll.rs/splice.rs 的全部 socket 特判**，`step_read/step_write` 的 socket arm 从 EINVAL 改为委派 ops；delegate→reactor 依赖收口到一个 port/trait；评估缓冲纳入 substrate 记账。

---

## 6. 代码规模真相（怀疑⑦）

<!-- txdoc:07-NET-AUDIT-LOC -->

**裁定：✗ 证伪"无用测试/死代码导致 4 万行"。** 膨胀来自**协议广度**，不是垃圾。

- 🔬 **亲验数字**（`grep`/`wc` 复核）：net/ `#[allow(dead_code)]`=**0**、`#[ignore]`=**0**、`todo!()`/`unimplemented!()`=**0**、`#[test]`=**249**、生产代码硬编码 `10.0.2.x`=**0**。
- 🔬 **修正一处自己的误判**：先前 `grep` 出"14 处 `10.0.2.x`"是**过滤 bug**（路径行首 `tests/` 未被 `/tests/` 模式排除）——14 处**全在** `tests/{bridge,nfnetlink,rtnetlink}_tests.rs`，生产代码确实 0。
- 🔬 **修正调查员一处数字**：全 `tx-subsystems` 的 `#[allow(dead_code)]` 调查员报 69，实测 **55**（口径差异，不影响"net/ 内为 0"的主结论）。
- 📋 **构成**：测试 ~34%（~14000 行 / 249 个 `#[test]` / 1527 断言 / 0 ignore = 健康）+ 真实生产 ~55% + netlink wire 样板 ~10%（`rtnetlink.rs` 2047 + `nfnetlink.rs` 1682 + `nfnetlink/` 551）。
- 📋 `cargo check -p tx-subsystems` 仅 1 条警告（`step_connect.rs:560` unused `guard`），0 条 dead_code。
- 📋 **唯一的规模隐患**（与 §2.4 呼应）：`payload.rs` 2392 行（183 fn / 127 match-arm）单文件承载 ~10 种协议状态联合，圈复杂度高——这是**设计耦合**点，不是死代码。

---

## 6-bis. 第二轮：主动发现（质量 / 安全 / 正确性）

<!-- txdoc:07-NET-AUDIT-ROUND2 -->

**缘起.** 使用者问"如果你去检查，还可能有哪些问题"。前 10 条集中在架构/数据路径；本轮派 4 个调查员专扫**前轮未碰的质量/安全/正确性**维度（并发锁 / 资源泄漏 / 不可信输入 / readiness），方法同样"多 agent 取证 + orchestrator 逐条核验行号"，并对 agent 间分歧做了裁决。

**头条（好消息）.** 📋 **解析不可信输入的崩溃类问题——全维度干净**：8 个解析文件 0 个路径 `unwrap`/`panic`，长度门 + `checked`/`saturating` 算术齐全，netlink TLV 与 IPv4 分片重组两个经典攻击点防御正确。**本栈在内存安全/抗崩溃上质量很高**——这条预判被证伪。

**核心观察.** 本轮发现几乎全是 §1/§2/§5 病根（无持久 Interface + 时钟冻结、双份机制、socket 状态切成 ~15 把锁、wait 两套注册表、手工旁路 smoltcp）在质量维度的**并发症**，而非独立新病。

### R1. 并发安全（SMP-only）

**前提**：默认 `-smp 4` 真并行；`SpinMutex` 纯自旋不可重入；socket 无顶层锁、逻辑状态切成 ~15 把独立锁。对手 = 用户 syscall step（hart A）∥ delegate ingest task（hart B）操作同一 socket。**单 hart 跑不出，`-smp 4` 命中。**

| # | 分级 | 位置 | 问题 |
|---|---|---|---|
| R1a | 高风险·SMP | 🔬`step_recv.rs:55-57,105-107` | **边沿就绪丢唤醒**：reader 用陈旧 `became_empty`（rx 锁已释放）在锁外 `clear_recv(HAS_DATA)`、无 recheck-refire；delegate 在"弹空→清位"间 ingest+fire 则就绪位被抹 → 缓冲有数据但读者永不醒 = **卡死** |
| R1b | 高风险·SMP | `payload.rs:1295-1301` | `refresh_io_from_raw` 在 io 锁外重算 recv/send，~40 并发调用点丢更新 → 就绪缓存撕裂，喂大 R1a |
| R1c | 高风险·SMP | `step_send.rs:363-370` | 双通路判定读 `protocol`+`protocol_state` 两把独立锁，握手期可翻转 → TCP send 错选 loopback/smoltcp 通路 |
| R1d | 高风险·SMP | `tcp.rs:185-200,214-272` | 发送侧跨 6 锁无复合原子；`enqueue` 无条件 `corked.clear()` → 共享 fd 并发发送/撞 cork flush 时**出站字节丢失/错乱** |
| R1e | 高风险·SMP | `step_bind.rs:151-160,283-314` | bind 通配/具体重叠 + reuseaddr 替换是 check-then-act → bind 排他性破坏/端口瞬时失绑 |

🔬 R1a 已亲验代码事实。**干净方面（agent 诚实排除）**：无 ABBA 死锁（锁序一致）、无持锁跨 await、无同锁重入、原子序全对（同步标志 Acquire/Release，`Relaxed` 仅统计计数）。

### R2. 资源泄漏 / OOM / 老化失效

| # | 分级 | 位置 | 问题 |
|---|---|---|---|
| R2a | 确认（根源） | `step_*` 全 `Instant::ZERO` | **时钟冻结 → ARP/半连接/分片/conntrack/TIME_WAIT 老化全失效**，表项只增不减 = 泄漏系统性根源（= §1 后果） |
| R2b | 高风险 | 🔬`step_socket_close.rs:48-51` | 监听 socket 关闭**不排空 accept backlog**，已完成待 accept 的子 socket（连接表强 Cap）泄漏（320KB + identity + ns 引用 + 连接槽） |
| R2c | 高风险 | `step_tcp_cleanup.rs:68-69` | cleanup 撤销全局初始-ns 表而非 socket 所属 netns 表 → 非初始 netns 连接回收落空 |
| R2d | 确认 | `tcp.rs:542-543`+`types.rs:724-725` | 每 TCP socket 预分配 320KB（loopback 不用仍分配）；且**无 socket 内存记账/全局上限** → 海量 socket 堆耗尽 panic |
| R2e | 确认 | `netfilter.rs:138-139` | conntrack 两表无上限无老化 + O(n) reply 查找 → 无界增长 + 每包二次退化 |
| R2f | 确认 | `payload.rs:35`↔`table.rs:117-129` | namespace↔socket 强引用环：未经 close 的 socket（如 R2b）钉死 namespace |

🔬 **B 冲突裁决（两 agent 分歧，已亲验 `namespace.rs:314-329`）**：`SocketTable` 的 `Box::leak` **有配套 `Drop` 回收，不是永久泄漏**（`table.rs:20` "never freed" 注释已过时）；真正不回收的是接口名（`rtnetlink.rs:1952`）+ 设备本体（veth/bridge/dummy/vlan）。另：该 Drop 的 SAFETY 注释**推理是错的**（称 Index 无 Drop，实则 `index.rs:216-232` 有）——实际仍内存安全，但注释误导维护者。

### R3. 不可信输入 / 校验

| # | 分级 | 位置 | 问题 |
|---|---|---|---|
| R3a | 确认·correctness | 🔬`smoltcp_demux.rs:24/42/63` | 硬件 RX 主路径只 `new_checked`（校长度）**不验 IPv4/TCP/UDP 校验和**；loopback/ICMP 却用 `Repr::parse` 验了 → 不一致，损坏数据被当合法包投递（非崩溃；NIC 硬件校验部分缓解） |
| R3b | 确认·低危 DoS | 🔬`ether.rs:790-792` | 分片重组流表满 64 触发 `fragments.clear()` 清空所有在途重组 → 65 个伪造源首片即可冲掉合法重组 |
| R3c | 低危 | `nfnetlink/wire.rs:507-531` | netlink 属性"猜端序"启发式（LE/BE 都解再选小），易误判 |

**干净**：解析路径无 panic/越界/整数 wrap（见头条）；启动/内部不变量的 `unwrap` 不在网络输入路径（低危）。

### R4. readiness / epoll 通知语义

| # | 分级 | 位置 | 问题 |
|---|---|---|---|
| **R4a** | **确认(代码)/高风险(运行)** | 🔬`epoll.rs:117-124,334`+`wait.rs:48`+`identity.rs:93-95` | **epoll 对 socket 无法阻塞——wait-source 两套注册表错配**：socket carrier 注册在 subsystems `REGISTRY`（`tx-subsystems/.../wait_source.rs:60`），epoll 用 substrate `lookup_source`（`tx-substrate/.../wake/wait_source.rs:444`）查 → 查不到 → `epoll_wait` 对未就绪 socket 立即返回 0（连超时不等）→ 无限超时退化为忙转。**是 ⑥ 的具体后果** |
| R4b | 高风险 | `step_poll.rs:197-255`+`epoll.rs:117-124` | epoll 每 entry 只订阅单一 carrier，IN 优先掩盖 OUT-only 唤醒（ppoll 无此问题，它取 IN+OUT 两个） |
| R4c | 高风险 | `epoll.rs`（无 `drive_loopback_pending`） | epoll 算就绪前不推进待处理网络事件，漏队列中数据 |
| R4d | 高风险 | `step_process_network_events.rs:288-290` | 真实设备 TCP recv 缺自愈 fire（loopback `poll_context.rs:515-519` 有）；是 R1a 竞态唯一缺兜底的暴露面 |

🔬 R4a 已亲验（两个独立 `static REGISTRY` 不同 crate/类型 + net 0 处 `register_source` + socket 用 `register_wait_queue` + epoll 用 `lookup_source`，错配坐实）。**干净方面（agent 诚实）**：ppoll/pselect 双向丢唤醒安全（取 IN+OUT carrier + 循环顶重算 + peek 复检）；recv/accept/send/connect 阻塞丢唤醒安全；第一轮"`accept_pending` 不刷新"线索经核实**无害**（三处绝对赋值 + `accept_wq` 兜底）。

### 两个 agent 分歧的裁决（诚实留痕）

**R1a（并发 agent）= readiness agent 的 3.4**："became_empty 锁外 clear"竞态。并发 agent 定 **高风险·SMP-only**（delegate 与 syscall 在不同 hart 真并行）；readiness agent 定 **理论**（协作式单线程不交错，仅独立 delegate/IRQ 并发才真实化）。**裁决**：代码事实确凿；是否触发取决于 reactor 是否把 net delegate task 与用户 syscall 调度到**不同 hart**——SMP 真并行下即真 bug，故**重构按最坏情况修**（clear-then-recheck-refire，成本极低），R4d 的设备路径自愈是关键兜底。

---

## 7. 重构方向汇总

<!-- txdoc:07-NET-AUDIT-REFACTOR -->

**一个先行决策（决定其余一切）.** smoltcp 的定位二选一：
- **(A) 拥抱 smoltcp**：建持久 per-iface `Interface` + 真实时钟周期 `poll()`，loopback 也喂给 smoltcp 的 loopback device，**删掉直拷旁路与 staging 双份**。收益：重传/握手/老化"免费"复活，单一数据所有者。
- **(B) 彻底自研**：删掉 smoltcp socket，自己实现 TCP 状态机（对齐 RawRds 形态）。收益：完全掌控；成本：要自写重传/拥塞控制（工作量大）。

> 审计建议倾向 **(A)**：栈已重度依赖 smoltcp 的 wire 与状态机，且 (A) 一举解决 §1（时钟/重传/握手）与 §2（双份）两大债。

**按依赖顺序的重构主线（每条都指向上文证据）.**

| 优先级 | 主线 | 解决 | 关键支点 |
|---|---|---|---|
| P0 | 持久 Interface + reactor 真实时钟 `poll()` | §1 全部 | 废 `with_context`；`next_deadline` 取 `iface.poll_at()` |
| P0 | 数据所有者单一化（删直拷旁路/staging 或删 smoltcp socket） | §1.3, §2.1-2.3 | 由先行决策定向 |
| P1 | `SocketPayload`：9 `Option` → `enum SocketImpl`；TCP 字段内聚 | §2.4, ⑨ | 现成样板 RawRds/RawPacket |
| P1 | 分层拆分 + 统一 `trait Interface` + 路由统一 FIB | §4 | `ether.rs` 拆 link/l2/l3 |
| P1 | IPv6 随持久 `Interface`/统一 trait 一起复活（**不单独补第二套**） | §3-bis, §1, §4 | smoltcp `Interface` 统一 v4/v6 邻居/路由 |
| P2 | `FileOps` trait 取代 socket syscall 特判 | §5.1 | 对照 `CharDeviceBinding.ops` |
| P2 | `IpEndpoint→enum{V4,V6}`；`SocketOptionSet` 分层；删 `last_syn_ack` | §2.3-2.4 | — |
| P3 | wait API 统一到 `WaitSource`（删 legacy WaitToken）；delegate→reactor 收口 port；缓冲纳入 substrate | §5.2-5.3 | 机制已统一，仅去迁移债 |
| P1 | **epoll 就绪统一到 socket 的 wait-source 路径**（修两套注册表错配） | §6-bis R4a | socket carrier 桥接进 substrate 表，或 epoll 改用 subsystems `wait_on_token`；与 ⑥ 同根 |
| P2 | **socket 单锁 + 就绪态单一来源、电平触发、持缓冲锁内派生** | §6-bis R1a/b/d | 与 ⑨ 的 `enum SocketImpl` 内聚一起做 |
| P2 | 真实时钟使老化/超时回收复活（ARP/半连接/分片/conntrack/TIME_WAIT） | §6-bis R2a | = P0 持久 Interface 的连带收益 |
| P3 | 资源回收补全：监听关闭排空 backlog、cleanup 用所属 ns 表、接口名/设备弃 `Box::leak`、conntrack 加界+老化 | §6-bis R2b/c/e | — |
| P3 | RX 验校验和（与 loopback 对齐）+ 分片表改 LRU 驱逐；修正 `namespace.rs` Drop 的错误 SAFETY 注释 | §6-bis R3a/b, R2(B裁决) | — |

**明确不必做的（防过度重构）.** 删测试（健康）、清"死代码"（近零）、改 net core 与 fd 表/进程的边界（已健康）、重写 ICMP/RawRds（已干净）。

---

## 附录 A. 关键证据索引（按 file:line）

<!-- txdoc:07-NET-AUDIT-EVIDENCE -->

| 主题 | file:line | 核验 |
|---|---|---|
| 一次性 Interface / 时钟冻结根因 | `protocol/tcp.rs:712-720` | 🔬 |
| 生产 15 处 `Instant::ZERO` | `execution/step_*.rs`, `namespace.rs:1778` | 🔬 |
| 内核真实时钟（net 未接） | `wall_clock.rs:128`；`init/net.rs:227-230` | 🔬/📋 |
| ARP "10 年永不过期" workaround | `protocol/ether.rs:392,408` | 🔬 |
| 唯一"重传"=loopback SYN-ACK 保活 | `execution/step_tcp_backlog_poll.rs:51-70` | 📋 |
| 外部 RX 丢 seq/ack | `packet/demux.rs:31-36`；`step_process_network_events.rs:275-324` | 📋 |
| 外部 connect 不发 SYN | `execution/step_connect.rs:58-118` | 📋 |
| TCP 双通路切换 | `execution/step_send.rs:363-426` | 🔬 |
| RawTcpSocket 5 缓冲 + last_syn_ack | `protocol/tcp.rs:24-33,27` | 🔬 |
| SocketPayload 9 `Option` 平铺 | `structure/payload.rs:33-53` | 🔬 |
| IpEndpoint v4+v6 混装 | `structure/types.rs:255-261` | 📋 |
| 干净样板 RawRds/RawPacket/RawSctp | `structure/payload.rs:1392/1499/1642` | 📋 |
| UDP smoltcp socket 死重 | `protocol/udp.rs:20-29`；`step_socket_close.rs:154` | 📋 |
| EtherIface 融合三层 | `protocol/ether.rs:121-133` | 🔬 |
| RX 重复编解码/重封装 | `protocol/ether.rs:240,255,263-268,274` | 🔬 |
| 无 Interface trait / 两套平行栈 | `device.rs:73`；`ether.rs:121` vs `loopback.rs:19` | 📋 |
| 路由分裂 FIB vs per-iface | `namespace.rs:977-983`；`ether.rs:283,1218` | 📋 |
| device 泄漏（bridge→netfilter 等） | `device/bridge.rs:11-13`；`device.rs:86-108`；`device/virtio.rs:7-8` | 📋 |
| socket 在 VFS 结构内 | `vfs/structure.rs:560-563` | 📋 |
| step_read/write 对 socket 返 EINVAL | `vfs/execution.rs:396-399,649-652` | 📋 |
| socket syscall 特判全表 | `io.rs:1946/2229`；`fs_basic.rs:1516`；`epoll.rs:119`；`splice.rs:69` | 📋 |
| CharDevice 多态对照 | `vfs/execution.rs:382,636` | 📋 |
| net 等待用统一 substrate bus | `net/structure/readiness.rs:30-34`；`tx-substrate/src/wake/wait_source.rs` | 📋 |
| delegate→reactor 具体类型耦合 | `net/delegate/{timer,runtime}.rs:2` | 📋 |
| 规模数字（dead_code=0 等） | `grep`/`wc`/`cargo check` | 🔬 |

---

*本文为 `feature-network-refactor@fd64ba24` 的现状快照。所有 `file:line` 可按此 HEAD 复核。调查员原始取证笔记见会话 scratchpad（不入库）。*
