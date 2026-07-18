# P3-B 执行计划：模型瘦身 —— enum SocketImpl + 每 socket 单锁 + 就绪单源

<!-- txdoc:07-NET-P3B-V1 -->

> 阶段来源：[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md) §5-P3 之②（模型瘦身：9×Option→enum、D4 单锁、D7 就绪单一来源）。前序 [`REFACTOR_P3_v1.md`](REFACTOR_P3_v1.md)（P3-A 上半接入）已收官。取证：两路并行调查（SocketPayload 迁移面 / 锁分布与就绪派生）+ orchestrator 亲验 D4/D7 权威表述。所有 file:line 按 `50579e55`。

---

## 1. 病根与目标（审计⑨②、R1b/R1d，方案 D4/D7）

**⑨ 9 槽平铺**：`SocketPayload`（payload.rs:33-53）用 9 个并列 `Option<RawXSocket>`（raw_tcp/udp/icmp/unix/rds/sctp/packet/netlink_route/netlink_netfilter）表达"这个 socket 是哪种协议"，于是 payload.rs 内布满 2~8 元组 match（8 处）与 early-`if let Some` 旁路（19 处）。一个 socket 只可能是一种协议——`enum SocketImpl` 让非法状态（双 Some/全 None）在类型上不可表达。**利好事实（取证）**：kind→槽是**无例外满射**（11 个 SocketKind 各激活恰一个槽；UnixDatagram/UnixStream 共 raw_unix、NetlinkXfrm/Netfilter 共 raw_netlink_netfilter）；`Packet` 槽在大 match 外单独填充（payload.rs:79，归一点）；重写面**全部集中在 payload.rs 单文件**（约 63 触点）；5 个访问器（raw_tcp_socket 等，payload.rs:443-474）保签名则外部 **72 处调用（26 文件）零改动**；测试零 struct 字面量构造（全经 step 助手+访问器）。

**② 状态记账现状（P1 后复核）**：三处记账已收敛到各司其职——`SocketProtocol::Tcp(TcpState)` = syscall 级意图 FSM（110 处 + Sctp 复用 84 处）、smoltcp `state()` = 线级真值、`RawTcpProtocolState` 只剩 2 个粘滞位且已封装在 RawTcpSocket 内（tcp.rs:33-36，随 variant 搬运零成本）。**本波裁量：`SocketProtocol` FSM 不动**——它被 Sctp 复用、194 个读写点跨 32 文件，收进 variant 的解耦成本远超收益，且它与槽 enum 的判别语义重复问题可在访问器层消化。

**R1d 跨锁发送竞态（实锤复核）**：TCP 一次 send 在 `corked_tx`/`socket`/`io` 三把锁上做 **6-9 次独立取放**（tcp.rs:192-247 逐步核对）：`send_available`（tcp.rs:205）读完释放→`send_slice`（tcp.rs:234）用**旧值**；corked 读取（tcp.rs:230）与 `clear()`（tcp.rs:239）之间并发 MSG_MORE 追加会被**连带清空丢字节**。UDP 侧同族（udp.rs:281-333，容量判定与实际入队非原子）。

**R1b 就绪缓存撕裂 + D7 多源**：就绪现有**三个来源**——① io snapshot（`refresh_io_from_raw` payload.rs:1317-1323，锁外读-算-写非原子，**37 个调用点**）；② readiness wire 位（fire_* 散落 **10 个生产文件**，仅 step_send.rs 就 13 处 fire_recv）；③ accept_pending（backlog 路径独立写，payload.rs:1072/1118/1126，backlog 与 io 两锁非原子）。step_poll 用 `io.X>0 || wq.peek()&BIT` 双源 OR（重复 8+3 处），事件处理层还有第三种组合（`bits.recv_readable || peek()==0 && recv_available()>0`，恰 2 处）。**判决性事实**：io snapshot 的生产唯一读方是 step_poll（FIONREAD 全仓无实现）——缓存可以**整体删除**而非修补。

**锁序卫生（利好）**：CONTEXT_IFACE 外层 / socket 内层的声明锁序（tcp.rs:612-616）全库 6 个 `with_context` 调用点一致遵守，无反序点、无死锁隐患——重构不需要先还锁序债。

---

## 2. 迁移面度量（工作量标尺）

| 面 | 数量 | 位置 |
| --- | --- | --- |
| 元组 match | 8 处（2~8 元） | 全在 payload.rs（L627/673/817/854/897/1143/1191/1223） |
| 内部触点合计 | ~63 | 全在 payload.rs |
| 保签名访问器 | 5 个 | 外部 72 调用点/26 文件零改动 |
| RawTcp 锁 | 3 把 → 1 | tcp.rs:25-27（socket/protocol_state/corked_tx） |
| RawUdp 锁 | 3 把 → 1 | udp.rs:36/37/42（socket/corked_tx/tx_src_hint） |
| refresh_io_from_raw 调用点 | 37 → 0 | payload.rs ~19 + execution 若干 + netlink 7 |
| step_poll 双源 OR | 8+3 处 → 单源 | step_poll.rs |

---

## 3. 分步实施（S1–S4，每步独立编译/验证/提交/可回滚）

### S1 —— `enum SocketImpl`：9 槽收敛（⑨）

- payload.rs 内：`enum SocketImpl { Tcp(RawTcpSocket), Udp(RawUdpSocket), Icmp(RawIcmpSocket), Unix(RawUnixState), Rds(RawRdsState), Sctp(RawSctpState), Packet(RawPacketSocket), NetlinkRoute(RawNetlinkRouteState), NetlinkNetfilter(RawNetlinkState) }`，`SocketPayload` 的 9 个 Option 字段换成 `imp: SocketImpl`（构造时即定型，无 Option）。
- 8 处元组 match → 单臂 match；19 处 early-guard → `if let SocketImpl::X(..)`；Packet 的场外填充（L79）归一进构造 match。
- 5 个访问器保签名（内部改 `match &self.imp { SocketImpl::Tcp(s) => Some(s), _ => None }`）。
- **不搬**伪公共字段：`tcp_backlog` 服务 TCP/SCTP/UnixStream 三族 listener、`ip_multicast` 服务 UDP+RawIcmp——都是真·多协议共享，硬塞 variant 反而复制；`unix_peer_cred`（仅 Unix）可搬但收益一行，随手做或留账均可。
- **验证**：纯结构重构，编译器当迁移向导；host 集合差 + 全冒烟矩阵不退化即判过。

### S2 —— D4 落点：每 socket 单锁（R1d 结构性消除）

- `RawTcpSocket` 三锁并一：`inner: SpinMutex<TcpInner { socket: Box<tcp::Socket>, protocol_state: RawTcpProtocolState, corked_tx: Vec<u8> }>`——`send_available→combine→send_slice→corked.clear→became_full` 全程一把锁内，**旧值窗口与 corked 覆盖窗口在构造上不可能**。UDP 同构（socket+corked_tx+tx_src_hint 并一）。
- 全部 pub fn 签名不变（锁是私有字段、API 全 `&self`——取证确认）。`with_context` 锁序不变（CONTEXT_IFACE 外层，inner 锁在闭包内取）。
- **临界区纪律（取证给出的红线）**：单锁只覆盖"组合+入环+就绪派生"复合原子；**不得**把 `with_context` 整段或 delegate 批处理循环纳入，否则事件循环退化全局串行。
- **与 v2-D4 端态的关系（设计点①）**：v2 的理想端态是"每 iface 一把锁"（asterinas 形态）——那要求 per-netns Interface 重设计，属 P5 邻接工程。本步交付的"每 socket 复合原子"消灭 R1d 的全部实测窗口；iface 级归并挂账到 P5，届时 socket 单锁可平移为 iface 锁的内层数据。
- **验证**：R1d 的两个窗口由构造消除（结构性论证写入代码注释）；行为面 = 全冒烟 + bulk 32KB（发送路径压力）；`-smp 4` 并发压力见证**挂账多核环境**（用户本机多核不稳，与 D14b 同列 LTP/多核轮）。

### S3 —— D7 就绪单源：删 io 缓存，poll 即时派生

- 删 `SocketIoState`/`io` 锁/`refresh_io_from_raw` 及其 37 个调用点；`step_poll_ready` 改为**持 raw 锁内即时派生**（S2 后一把锁天然原子）：recv = `raw.recv_available()>0 || wire BROKEN 位`，send/accept 同构；accept_pending 从 backlog 锁内直读（唯一写路径已在该锁内）。
- wire 位（fire_*/RawQueue/双表镜像）**全保留**——D7 只统一"就绪怎么算"，唤醒机制是 P3-A S1 的地盘；三种重复派生组合（8+3+2 处）收敛为 poll 一处 + 事件层一处助手。
- 电平语义自证：poll 每次从真值即时算，不存在"缓存说有、环里没有"的撕裂（R1b 结构性消除）。
- **验证**：poll 行为判决单测（就绪/未就绪/BROKEN 三态 × tcp/udp/listener）；epoll/ppoll 冒烟（采样路径改动的直接见证）；全矩阵 + 集合差。

### S4 —— 清扫收官

- 死代码：io snapshot 残留、无主访问器、netlink 侧 7 处 refresh 调用的等价替换核对。
- 文档：v2 §3 D4/D7 补"落点已实现（socket 级）/端态挂 P5"注记；STATUS/记忆同步。
- 终验矩阵：七冒烟 + accept + 集合差 + busybox-boot + la64。

---

## 4. 测试方案

1. **判过标准**：本波是行为保持型重构——host `tx-subsystems --lib` 集合差（基线 + 已知新测试名清单）与全冒烟矩阵（ext/tcp-lo/udp-lo/dns/seq/bulk/epoll/accept）不退化即判过；S3 另有 poll 三态判决单测。
2. **结构性消除的论证义务**：R1d/R1b 的修复以"窗口在构造上不可能"论证（临界区代码注释 + 计划引用），`-smp 4` 压力实证挂账多核环境轮——**不以"跑了没炸"冒充并发正确性证明**。
3. **暂缺**：LTP net 全量（无镜像）；loom/并发模型检查（无基建）——均记 STATUS。

---

## 5. 风险与已知坑

1. **S1 是 63 触点的机械重写**：靠编译器驱动，但 payload.rs 同时是 P2-S6/P3-A 刚动过的热文件——重写前先跑一次基线集合差存档，防叠加漂移误判。
2. **S2 粗锁热点**：per-socket 锁合并后临界区变长（combine+send_slice 一体）；单 netns TCG 下无感，多核真机场景的锁竞争随 `-smp 4` 见证轮再评。红线：不把 with_context/delegate 循环纳入。
3. **S3 删缓存后 poll 直取 raw 锁**：poll 频率高（epoll 每次唤醒重采样）；临界区极短（读几个计数），可接受；若未来 FIONREAD 实现，同一派生函数直接复用。
4. **回滚单元 = S 步**；S1 与 S2/S3 相互独立可分别 revert。

---

## 6. 设计点拍板（按既定授权取推荐）

| # | 问题 | 取向 |
| - | --- | --- |
| 1 | D4 落点 | **每 socket 单锁（本波）**；每 iface 锁 = 端态，挂 P5（需 per-netns Interface 重设计），届时 socket 锁平移为内层数据 |
| 2 | variant 内字段搬迁 | **只收 9 槽**；tcp_backlog/ip_multicast 真·多协议共享不搬；SocketProtocol FSM 不动（Sctp 复用 + 194 触点） |
| 3 | io 就绪缓存 | **整体删除**（唯一读方是 step_poll，FIONREAD 无实现）而非修补 refresh 的原子性 |
| 4 | Unix/RDS/SCTP/Packet 的 Raw 状态体 | 原样入 variant（自带内部锁不动）——它们不在 R1 竞态热路径上，锁收敛只做 TCP/UDP |

---

## 7. 实施记录（2026-07-03 完成，S1–S4）

<!-- txdoc:07-NET-P3B-V1-DONE -->

- **S1**（ed037dc2）9 槽 → `enum SocketImpl`：18 字段 struct 收成"公共字段 + `imp: SocketImpl`"；构造 match 从九元组瘦成 `(protocol, imp)` 对，`Packet` 场外填充归一、`NetlinkXfrm/Netfilter` 合并臂；8 处元组 match（2~8 元）全收敛单臂、19 处 early-guard 改经 `SocketImpl` 访问器。5 访问器保签名 → 外部 72 调用点零改动。重写面如取证全落 payload.rs 单文件。
- **S2**（92862a45）每 socket 单锁：`RawTcpSocket` 三锁（socket/protocol_state/corked_tx）并入 `inner: SpinMutex<TcpInner>`，`RawUdpSocket` 三锁（socket/corked_tx/tx_src_hint）同构。R1d 双窗口（stale-available、corked read→clear 覆盖）构造性消除——`available→combine→send_slice→clear` 一次锁获取。`process_segment` 的 socket→drop→protocol_state 两段锁舞消失。内部套用改 inner 级自由函数避免自锁。pub API 全保签名，锁序不变。
- **S3**（b07b8f98）就绪单源：`io_snapshot` 从读缓存改实时派生（recv=raw_recv_available/send=raw_send_available/accept=backlog.connected_len）；删 `io` 字段 + `refresh_io_from_raw` + 27 调用点 + 3 处 accept 缓存写。R1b 撕裂随缓存消失。**过程抓修一个自锁死锁**：io_snapshot 实时派生内经 `raw_recv_available→with_protocol` 重锁 `protocol`，而 `step_poll_ready` 在 `with_protocol` 闭包内调 io_snapshot → SpinMutex 自旋死锁（dns recvfrom 挂起 / epoll_pwait 不返回）；修法=step_poll_ready 把 io_snapshot 提到 `with_protocol` 前算一次。宿主级冒烟二分定位。
- **S4**（本提交）清扫：删无引用的 `SocketIoState::new()`（保 struct + Default）；文档/STATUS/记忆同步。

**验收**：每步 host 集合差 312=312 全同 + 六冒烟（ext/tcp-lo/udp-lo/dns/seq/epoll）+ accept + bulk 32KB；la64 构建、busybox-boot 过。**R1 并发家族**以"窗口构造性不可能"论证交付，`-smp 4` 并发实证挂多核环境轮（本机多核不稳，与 D14b 同列）。

---

*P3-B 完成：socket 模型 = 一个 enum、每 socket 一把锁、就绪一个来源；P3-C（R 族修复：R1c/e 残余、R2b/c/d/e/f 资源回收）在干净地基上另立执行文档。*
