# 网络栈重构 P1 执行计划 v1：loopback 收敛 smoltcp 单通路 + 删双份缓冲

<!-- txdoc:07-NET-P1-V1 -->

**Status.** v1.1 (2026-07-02)。[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md) 阶段 **P1** 的可执行细化。前置：P0 已完成（`5ab58517`，见 [`REFACTOR_P0_v1.md`](REFACTOR_P0_v1.md) §7）。§6 四设计点已拍板全 A；**S0–S5 已全部实施并提交**（`2fc0a3ea…402a441d`），验证记录与范围修订见 §8。

**Purpose.** 把 loopback 的 TCP/UDP 数据路径收敛成**唯一的 smoltcp 段级通路**，删除直拷旁路与双份缓冲——修审计 ①（loopback 重传）②（双通路/双份记账）③（UDP 旁路）的 loopback 部分。**不碰**：外部握手（P2）、FileOps/等待机制（P3/D13/D14）、IPv6（P4）、`SocketPayload` 9×Option（P3）。

**基准.** 工作树 `feature-network-refactor @ 5ab58517`（P0 之后）。所有 `file:line` 按此 HEAD；路径省略前缀 `crates/tx-subsystems/src/net/`。

**给现场赛的读者.** §1 是本文最重要的部分：现状不是审计粗颗粒说的"loopback 全靠直拷"，而是**三条 TCP 通路 + 两条 UDP 通路并存**。把这张地图看懂，改动清单（§3）就只是按图拆墙。

---

## 0. 一句话

<!-- txdoc:07-NET-P1-V1-ONELINE -->

> loopback 的 TCP **握手和段级传输其实已经走真 smoltcp**（P0 解冻后重传也活了），病根是旁边**还立着两条旁路**（TCP 直拷流 + UDP 直拷）和**一套影子缓冲**（用户读写永远经过旁挂 `rx_buffer`/`tx_buffer`，smoltcp 的 ring 只当中转站）。P1 = 拆旁路、废影子：**数据只住 smoltcp ring，用户读写直达 `recv_slice`/`send_slice`，loopback 传输只有段级一条路**。

---

## 1. 现状全景（先把地图画对）

<!-- txdoc:07-NET-P1-V1-MAP -->

### 1.1 TCP：三条通路并存

**通路选择器**：`tcp_uses_direct_stream`（`execution/step_send.rs:363-370`）——按 `RawTcpProtocolState.has_connected` 分流，分发点在 `step_send.rs:123-124` 与 `:266-267`：

```rust
fn tcp_uses_direct_stream(payload: &SocketPayload) -> bool {
    matches!(..., TcpState::Connected { .. })
        && payload.raw_tcp_socket()
            .is_some_and(|raw| !raw.protocol_runtime_state().has_connected)
        //                     ^^^^ smoltcp 没握手过的连接 → 走直拷 A
}
```

**通路 A —— 直拷流（要删）**。`send_tcp_stream_bytes`（`step_send.rs:389-426`）：查表找对端 → `record_tcp_stream_bytes`（`structure/payload.rs:634-636`）→ `ingest_rx_bytes_unbounded`（`protocol/tcp.rs:127-134`，**无容量上限**）直塞对端 `rx_buffer` → 当场 `fire_recv`（:423）。无段、无 seq/ack、无背压。**谁在 A**：所有"手工置 Connected"的连接——即外部 demux 造的 accept 子 socket（`step_process_network_events.rs:302-306` `create_connected_stream_for_accept_in_namespace`，不走 smoltcp 握手 → `has_connected=false`）。

**通路 B —— 段级 + 影子记账（保留骨架、删影子）**。loopback 正常 connect 走这里：

- 握手是**真 smoltcp 段**，同步内联驱动（`step_tcp_loopback.rs:302-352` `establish_smoltcp_loopback_on_iface`）：`connect_endpoint` 发 SYN → `poll_egress_one` → iface 队列 → `poll_ingress` 造 child → SYN-ACK → ACK，**两端必须真到 `State::Established`**（:335-340）。
- 数据段级 ping-pong（`step_process_loopback_tcp`，`step_tcp_loopback.rs:135-252`）：`poll_egress_one`（`protocol/poll_context.rs:57-76`，真 `dispatch_segment` → `emit_ipv4_packet` → `iface.dispatch_ip` 入队）+ `poll_ingress`（→ `process_segment` + `drain_protocol_recv_to_staging`，`poll_context.rs:489-532`）。
- **但影子记账贯穿全程**：发端字节同时住 smoltcp tx ring 和 `tx_buffer` 镜像（`tcp.rs:264-266`），传完 `take_tcp_tx_bytes(bytes_moved)` 平账（`step_tcp_loopback.rs:227`）；收端 smoltcp rx ring 的字节被 `drain_protocol_recv_to_staging`（`tcp.rs:489-509`）搬进 `rx_buffer`，用户才读得到。手工流控 `peer_space = recv_capacity - recv_available`（:181-184）与 smoltcp 窗口并存。
- 三个驱动点：发送内联（`tx-shims/linux_syscall/socket/helpers.rs:313`）、delegate 预算轮询（`execution/step_loopback_pending.rs:161`）、close 冲刷（`execution/step_socket_close.rs:192`）。

**通路 C —— 外部 RX 旁路（P1 顺手改喂真段，见 §6-2）**。demux 把线上的 TCP 载荷裸剥（`packet/smoltcp_demux.rs:41,57`）→ `process_tcp_event`（`step_process_network_events.rs:275-295`）：`record_recv_payload` 裸塞 `rx_buffer`，并按**收到的载荷长度**猜测性释放发送空间（`ack_bytes = max(1, payload_len)`，:287——根本不读 ACK 号）→ `ack_tx_bytes` 弹影子（`tcp.rs:311-315`）。smoltcp `process()` 全程不在场。

### 1.2 UDP：两条通路，smoltcp 完全不在场

`RawUdpSocket`（`protocol/udp.rs:20-29`）里的 smoltcp `udp::Socket` 是**摆设**——只被 `can_recv/can_send/close/set_hop_limit` 碰过（udp.rs:279-292, 75-77），数据全住自研 `rx_datagrams`/`tx_datagrams` 队列。

- **同步直拷**：`step_send_udp_loopback_kernel_bytes`（`execution/step_udp_loopback.rs:101-219`）——入 `tx_datagrams` 又立刻弹出 → `lookup_udp_ingress` 查对端 → `record_recv_payload` 直塞对端 `rx_datagrams` → 当场 fire（:210-217）。
- **延迟 iface 路**：`step_process_loopback_udp_on_iface`（:24-99）**先试直拷** `poll_udp_loopback_direct_one`（`poll_context.rs:107-186`，同样不经 iface），失败才手工 `emit_ipv4_packet` → `LoopbackIface` 队列 → 手工 `parse_ipv4_packet` → 还是塞 `rx_datagrams`（`poll_context.rs:78-105, 310-358`）。编解码用 `smoltcp::wire` 的 repr，但 socket 引擎不参与。

### 1.3 缓冲居所总表（谁写、谁读、删后归宿）

| 缓冲 | 写入方 | 读取方 | P1 归宿 |
| ---- | ------ | ------ | ------- |
| `rx_buffer`（tcp.rs:28） | ① `drain_protocol_recv_to_staging`（smoltcp中转，tcp.rs:505）② demux 旁路（payload.rs:626 ← events.rs:288）③ 直拷流（`ingest_rx_bytes_unbounded`，payload.rs:636 ← step_send.rs:419） | **用户 recv 唯一来源**：`step_recv` → `consume_recv_bytes[_into]`（payload.rs:650/659）→ `recv_len/recv_bytes`（tcp.rs:147/166） | **删**。读改 `socket.recv_slice`/`peek_slice`；写①随staging删、②改喂 `process_segment`、③随通路A删 |
| `tx_buffer`（tcp.rs:29） | `send_slice` 成功后镜像（tcp.rs:264-266, 296-298） | `send_available` 记账（:187）、`ack_tx_bytes` 弹（:311）、`dequeue_tx_bytes`/`take_tcp_tx_bytes` 平账（:326/payload.rs:973） | **删**。发送空间纯由 smoltcp 派生；平账逻辑随之消失 |
| `corked_tx`（tcp.rs:30） | MSG_MORE 暂存（tcp.rs:235-237），≥1460 自动冲（:238-239） | 冲刷进 `send_slice`（:250-262, 275-300） | **保留**（§6-3）：它是"未提交"staging，不是双份 |
| `last_syn_ack`（tcp.rs:27） | `dispatch_segment` 缓存 SYN-ACK（tcp.rs:440,531） | backlog 手工重传（`step_tcp_backlog_poll.rs:60-61`） | **删**（S3）：P0 解冻后 smoltcp 自己会重传 |
| `protocol_state`（tcp.rs:26） | `process_segment` 置 3 flag（tcp.rs:462-484） | `has_connected`=通路选择器（step_send.rs:369）等 | **瘦身**：`has_connected` 随通路A死；`is_recv_shut/is_rst_closed` 保留为小 flag |
| UDP `rx_datagrams`/`tx_datagrams`/`corked_tx`（udp.rs:22-24） | 两条直拷路 | `recv_datagram_bytes`（udp.rs:133-161）← payload.rs:725-740 | **删/收敛**（S4）：数据进 smoltcp `udp::Socket` |

另两个事实（P1 要用）：smoltcp ring **本来就按** `recv_buf_size/send_buf_size` 分配着（`tcp.rs:538-545`，默认 262144/65536——即现状是 ring+影子**双份内存**，R2d 的 loopback 份）；`SO_RCVBUF` 在构造后 setsockopt 不会改 ring 尺寸（构造时捕获，`tx-shims/socket.rs:2185-2213` 只写 options）——P1 不修，记为已知坑。

### 1.4 就绪与唤醒（P1 只换"信号来源"，不动机制）

唤醒真相是 `SocketReadiness{recv_wq,send_wq,accept_wq: RawQueue}`（`structure/readiness.rs:30-34`）→ `fire_*` → `RawQueue::fire`（`tx-substrate/bus/queue.rs:215-249`）置位并唤醒订阅者；等待方拿 `WaitToken{source_id=wait_carriers.*, interest}`（`execution/mod.rs:108-131`）。发布有两种形态：直拷路径**手工当场 fire**（step_send.rs:423 等），poll 路径经 `NetworkPublish::publish_to`（`packet/publish.rs:27-48`），其中 TCP 的位来自 `process_segment` 的前后状态 diff（`SmoltcpTcpProcessPublish`，tcp.rs:449-487 → `poll_context.rs:489-532` 消费，`connected` 位触发 accept 晋升 `poll_context.rs:612-650`）。**P1 后只剩 poll 形态**（单一来源，D7 方向）；`WaitToken` 挂起机制本身归 D14/P3，P1 不碰。`SocketIoState`（payload.rs:1318-1333）是另一份记账缓存（非唤醒机制），随影子缓冲一起简化。

### 1.5 P0 遗留的两处冻结时间戳

内联路径自建 `PollContext::new_with_table(Instant::ZERO, …)`：`step_tcp_loopback.rs:190` 与 `:319`。P0 只解了 `with_context` 的 `cx.now`（smoltcp 已看到真时间）；但 `PollContext.timestamp` 喂 backlog 的 `created_at`（`poll_context.rs:452`）——S5 一并换 `net_now_instant()`。

---

## 2. 目标形态（asterinas-true 的 loopback）

<!-- txdoc:07-NET-P1-V1-TARGET -->

```
            改造前（三缓冲六队列两旁路）                 改造后（数据单一居所）
用户 write ──► corked_tx/tx_buffer影子+smoltcp ring     用户 write ──► [MSG_MORE: corked_tx] ──► socket.send_slice
                │       │                                                     │ (smoltcp tx ring，唯一居所)
                │直拷A/UDP直拷                                                 ▼ poll: socket.dispatch(cx)
                ▼       ▼段级B                            真 IP 段/包 ──► LoopbackIface 队列（lo 设备）
          对端rx_buffer ◄─ drain_staging ◄ smoltcp rx                          │
                │                                                             ▼ poll: socket.process(cx,…)
用户 read ◄── rx_buffer/rx_datagrams                     用户 read ◄── socket.recv_slice（smoltcp rx ring，唯一居所）
```

不变量（P1 完成的判据）：
1. **数据单一居所**：用户可见的字节只存在于 smoltcp ring（+`corked_tx` 未提交暂存）。`rx_buffer`/`tx_buffer`/`rx_datagrams`/`tx_datagrams` 字段不复存在（编译期保证）。
2. **单通路**：loopback 上任何一个字节从 A 到 B 必然经过 `dispatch → LoopbackIface 队列 → process`。`tcp_uses_direct_stream`、`send_tcp_stream_bytes`、`poll_udp_loopback_direct_one`、`step_send_udp_loopback_kernel_bytes` 的直拷分支不复存在。
3. **流控 = smoltcp 窗口**：手工 `peer_space` 计算删除；对端 ring 满 → 窗口收缩 → 发端 `send_slice` 返回 0 → 阻塞/EAGAIN。`ingest_rx_bytes_unbounded` 的无界塞消失（这是行为**修复**，不是等价改写）。
4. **就绪单一来源**：所有 fire 都产自 poll 后的状态 diff（`SmoltcpTcpProcessPublish`/UDP 等价物）→ `NetworkPublish::publish_to`。
5. **驱动形态保留**：仍是"发送后内联驱动一轮 + delegate 兜底 + close 冲刷"三点（时延特性不变），只是内联驱动的内容从"专用对拷"变成"跑一轮该 iface 的 poll"。

---

## 3. 分步实施（S0–S5，每步独立编译、可验证、可提交）

<!-- txdoc:07-NET-P1-V1-STEPS -->

> P1 动的是所有 LTP net 测试压着的热路径，**一次性大改必死**。每步结束跑：`cargo test -q -p tx-subsystems --lib -- --test-threads=1`（对照 305 败基线集合差）+ QEMU `tcp/udp-loopback-smoke`。步序经过依赖排序：先断旁路（S0），再删缓冲（S1/S2），再动握手辅助（S3）、UDP（S4）、扫尾（S5）。

### S0 —— 断 TCP 直拷旁路（通路 A 之死）

- 删 `tcp_uses_direct_stream` + `send_tcp_stream_bytes`（`step_send.rs:363-426`）及两个分发点（:123-124, :266-267）。
- 删 `record_tcp_stream_bytes`（payload.rs:634-636）+ `ingest_rx_bytes_unbounded`（tcp.rs:127-134）。
- **为什么安全**：loopback 正常连接走真握手 → `has_connected=true` → 本来就在通路 B；落在 A 的只有外部 demux 手工造的连接，而外部 TCP 在本分支**结构性不可用**（审计①），删了不退化。手工 Connected 的连接 send 将走通路 B 的 smoltcp 路径并因未握手而失败——行为从"假成功"变"诚实失败"。
- **验证**：loopback LTP 子集不变绿；`grep -rn tcp_uses_direct_stream` 为空。

### S1 —— TCP 收端收敛（`rx_buffer` 之死）

- `consume_recv_bytes[_into]`（payload.rs:650/659）的 TCP 分支改调 smoltcp：读 `socket.recv_slice`、MSG_PEEK 用 `peek_slice`（fork 具备性见 §5-4 预检）。
- 删 `drain_protocol_recv_to_staging`（tcp.rs:489-509）及其调用（poll_context.rs:499——`became_readable` 改由 `can_recv` 的 diff 派生）。
- **通路 C 改喂真段**（§6-2 拍板 A 后）：`process_tcp_event` 已建立连接分支（events.rs:282-295）从 `record_recv_payload`+猜测性 `record_send_space` 改为构造 repr 喂 `process_segment`（demux 在 `smoltcp_demux.rs` 本就解析过头部）；SYN 分支原样留给 P2。
- 删 `ingest_rx_bytes/recv_available/recv_len/recv_bytes` 与 `rx_buffer` 字段本体；`step_recv` 的空判/清位改依 `can_recv()`。
- **为什么安全**：通路 B 的字节本来就先进 smoltcp rx ring（staging 只是搬运工）；删搬运工后用户直读同一份数据。容量语义等价（ring 本就 `recv_capacity` 大小）。
- **验证（本步判决性）**：单测——peer 发 N 字节后，**不经任何 staging**，`recv_slice` 直接吐出 N 字节；`recv_available()` 概念由 `socket.recv_queue()` 取代。**必须单跑 `recv01`/`recvfrom01`**（冷启动前科，见 §5-1）。

### S2 —— TCP 发端收敛（`tx_buffer` 影子之死）

- `enqueue_tx_bytes_with_more` 不再镜像（删 tcp.rs:264-266, 296-298）；`send_available` 纯由 `send_capacity - socket.send_queue()` 派生（改 :186-204）。
- 删 `ack_tx_bytes`/`record_send_space`（tcp.rs:311/payload.rs:643——通路 C 改造后无人再猜测性释放）、`dequeue_tx_bytes`/`take_tcp_tx_bytes`（tcp.rs:326/payload.rs:973）。
- `step_process_loopback_tcp` 简化：删平账行（step_tcp_loopback.rs:227）与手工流控（:181-187），`bytes_moved` 直接取自段级 ingress 统计；发送空间的 publish 由 `can_send` diff 派生。
- **验证**：send→recv 字节数守恒单测；对端不收包时发满 ring 后 `send_slice`=0（背压生效，替代旧手工 `peer_space`）。

### S3 —— 握手辅助收敛（`last_syn_ack` 之死 + `protocol_state` 瘦身）

- 删 `last_syn_ack`/`remember_syn_ack`/`retransmit_syn_ack_segment`（tcp.rs:27,440,446,531-534）；backlog 的 SYN-ACK 重传（`step_tcp_backlog_poll.rs:60-61`）改为对 child socket 直接 `dispatch_segment`——P0 解冻后 smoltcp 的重传定时器到期自会重发 SYN-ACK，backlog poll 只负责驱动 dispatch。
- `protocol_state` 删 `has_connected`（选择器已死）；`is_recv_shut`/`is_rst_closed` 保留（等价 asterinas 的 socket 旁挂小 flag）。
- **验证（判决性）**：**loopback SYN-ACK 丢失重传测试**——握手中途丢弃 SYN-ACK 段，拨 `NET_NOW_NS` +2s，poll 后 child 重发 SYN-ACK、握手最终完成。旧码靠 `last_syn_ack` 手工缓存，新码靠 smoltcp 定时器，行为等价且更正。

### S4 —— UDP 收敛进 smoltcp（旁路③之死）

- `RawUdpSocket`：bind 时同步 `socket.bind(endpoint)`；send 路径改 `socket.send_slice(payload, meta)`（进 smoltcp tx ring）；recv 改 `socket.recv()/peek()`（含 src 地址元数据，替代 `UdpRxDatagram.src`）。
- 删 `rx_datagrams`/`tx_datagrams` 与 `ingest_rx_datagram`/`recv_datagram_bytes`/`take_udp_tx_datagram` 一族；UDP `corked_tx` 保留（MSG_MORE 合包暂存，同 §6-3）。
- 删两处直拷（`step_send_udp_loopback_kernel_bytes` 的直塞段 `step_udp_loopback.rs:196-217`、`poll_udp_loopback_direct_one` `poll_context.rs:107-186`）；统一走 `poll_udp_egress_one`（改为 `socket.dispatch` 产包）→ iface 队列 → `poll_udp_ingress`（改为 `socket.process` 收包）。
- **为什么安全**：编解码本来就用 `smoltcp::wire`（udp.rs:296-352），换成 socket 引擎产/收同格式的包，线格式不变；就绪位从"手工 fire"变"`can_recv` diff"，时机由内联驱动保持同步。
- **验证**：UDP echo 单测走完整段级路（在 iface 队列上可观测到包）；`udp-loopback-smoke`；LTP UDP 子集。

### S5 —— 扫尾：时间戳 + 驱动形态 + 死代码

- 两处 `PollContext::new_with_table(Instant::ZERO, …)`（step_tcp_loopback.rs:190/319）→ `net_now_instant()`。
- 发送内联驱动点（helpers.rs:313）从"专用对拷步"改为"驱动一轮 loopback poll"（同步性保留，见 §6-4）。
- `grep` 扫尾：`ingest_rx|tx_buffer|rx_datagrams|direct_stream|drain_protocol` 全仓为空；删 `LoopbackTcpTransferOutcome` 中失效字段。
- **验证**：全量门（§4 第 4 层）。

---

## 4. 测试方案（四层）

<!-- txdoc:07-NET-P1-V1-TEST -->

1. **每步judgment单测**（见各 S 步"验证"）。其中 P1 灵魂测试是 **loopback 数据段丢失重传**（S1/S2 后可写）：建立连接 → 发 N 字节 → 从 `LoopbackIface` 队列**人为丢弃一个数据段** → 拨 `NET_NOW_NS` 越过 RTO → poll → 对端仍收齐 N 字节。直拷世界里"丢段"概念根本不存在，这个测试只有"段级单通路 + 活时钟"能过——它同时锁死 P0 与 P1 的成果。
2. **`tests/loopback_tests/*` + `net/tests` 全量**：按 305 败基线做 stash 集合差（方法论见 P0 §7）；直拷行为的既有断言（如依赖无界塞、假 ACK 记账的）按新语义**显式更新并逐条记录**。
3. **QEMU**：`tcp/udp-loopback-smoke` + busybox-boot 哨兵；**单跑** `recv01`/`recvfrom01`（§5-1）。
4. **不回归门**：LTP loopback TCP/UDP 子集对照 P1 前；`cargo -q xtask unit`（tx-shims/tx-ext4 既有失败不计）。

---

## 5. 风险与已知坑

<!-- txdoc:07-NET-P1-V1-RISKS -->

1. **冷启动假阳（前科）**：`recv01`/`recvfrom01` 曾在"单跑"时挂死于首次 connect（批量跑被前面测试暖过而假 PASS）。P1 改的就是这条路——**每个 S 步都要单跑这两个用例**，不能只信批量结果。
2. **就绪时机漂移**：直拷是"send 内同步 fire"，段级是"poll 后 publish"。保留内联同步驱动（§6-4 选 A）则用户可见时序不变；若选纯异步，阻塞唤醒延迟增加一次调度，LTP 超时敏感用例可能抖。
3. **背压是新行为**：`ingest_rx_bytes_unbounded` 无界塞变成窗口背压——修 bug，但若有测试依赖"无限塞得下"会红，属测试断言更新而非回归。
4. **前置预检（写码前先 grep fork）**：smoltcp fork 的 `tcp::Socket::peek_slice`、`udp::Socket::peek`、UDP `send_slice(meta)` 签名可用性；若 peek 缺失，S1 的 MSG_PEEK 需在 fork 补 pub 方法（fork 本就为手动驱动改过 pub）。
5. **`SO_RCVBUF` 构造后失效**：现状如此，P1 不修不改（ring 尺寸仍构造时定），记入已知问题清单。
6. **回滚单元 = S 步**：每步一个 commit，出问题回滚到上一步，绝不跨步混改。

---

## 6. 待拍板设计点（4 个）

<!-- txdoc:07-NET-P1-V1-DECISIONS -->

| # | 问题 | A（推荐） | B |
| - | ---- | --------- | - |
| 1 | `LoopbackIface` 去留 | **保留其包队列骨架**（它已是"lo 设备"雏形；`NetDeviceKind::Loopback` 变体已预留，device.rs:64-71）；"注册成正规 `NetDeviceOps` 设备、与 eth 同一 poll"推到 P2 统一 | P1 即改 NetDeviceOps 设备（改动面大，且 `NetDeviceOps` 是以太帧形状、lo 是 IP 形状，需先解决介质错配） |
| 2 | 通路 C（demux RX 旁路）在 P1 的处理 | **TCP 已建立分支改喂 `process_segment`**（外部 TCP 本来结构性不可用，不退化；且不改则 `rx_buffer` 有外部写入方、S1 删不干净） | 原样保留到 P2（则 `rx_buffer` 只能删一半，S1/S2 的"单一居所"不变量不成立——**实质不可行**） |
| 3 | `corked_tx`（MSG_MORE 暂存）去留 | **保留**（它是"未提交"字节的暂存，不是双份；smoltcp 无 cork 原语） | 删掉、MSG_MORE 退化为立即发送（语义损失，LTP sendmsg 族可能红） |
| 4 | 发送后的内联驱动形态 | **send 后同步驱动一轮 loopback poll**（时延特性与现状等价，asterinas 同款：send 即 poll iface） | 只 `kick_poll` 交 delegate 异步跑（代码更简，但唤醒多一次调度延迟，TCG 下 LTP 抖动风险） |

> 与 v2 D8 的口径差：D8 字面写"删 `LoopbackIface`"。本文按设计点 1-A 主张**P1 保留队列骨架、P2 设备化**——阶段边界更干净（P1=数据通路收敛，P2=设备/poll 统一）。若拍板 1-B，S4/S5 需相应加"介质适配"子步。

> **拍板（2026-07-02）：用户确认四点全按推荐（1-A/2-A/3-A/4-A）。** 实施按 §3 S0–S5 逐步进行，每步一个 commit。

---

## 7. 验收 / 提交 / 范围

<!-- txdoc:07-NET-P1-V1-DONE -->

- **验收**：§2 五条不变量成立；灵魂测试（丢段重传）绿；`recv01/recvfrom01` 单跑绿；LTP loopback 子集对照 P1 前不退化（305 基线集合差为空）；QEMU 双 smoke 绿。
- **提交**：每 S 步一个 commit（`net: P1-S0 断 TCP 直拷旁路` 依此类推），全程可回滚。
- **明确不做**：外部握手/外部 TCP 打通（P2）、loopback 设备化+统一 poll（P2，若 §6-1 选 A）、FileOps/等待机制（P3/D13/D14）、`SocketPayload` 9×Option（P3）、IPv6 数据路径（P4）、`SO_RCVBUF` 动态生效（另立小项）。

---

## 8. 实施与验证记录（2026-07-02，S0–S5 全部完成）

<!-- txdoc:07-NET-P1-V1-VERIFIED -->

**提交序列**（每步独立验证后提交，可逐步回滚）：

| 步 | commit | 内容 |
| -- | ------ | ---- |
| S0 | `2fc0a3ea` | 断 TCP 直拷旁路（选择器/直拷函数/无界塞全删） |
| S1 | `12183e11` | 删 `rx_buffer`，recv 直读 smoltcp ring（API 签名不变换后备存储）；通路 C 改喂 `process_segment`（demux 附带校验和验证过的完整段）；删假 ACK 记账 |
| S2 | `3553fac1` | 删 `tx_buffer` 影子，`send_available` 纯 ring 派生，背压归 smoltcp 窗口，删平账/手工流控 |
| S3 | `baca7045` | 删 `last_syn_ack`，SYN-ACK 重传归 smoltcp 定时器（backlog 闭包语义修正：RTO 未到 ≠ 失败）；`has_connected` 闩锁改 before/after 边沿 |
| S4 | `123faf5d` | 杀 UDP 两处直拷，loopback UDP 一律经 lo 队列真包转运；UDP emit/parse 补 v6 臂；**修 SMP 竞态**（见下） |
| S5 | `402a441d` | 内联路径 5 处 `PollContext(ZERO)` 时间戳解冻；死代码清扫 |

**灵魂测试**：`tcp_loopback_lost_data_segment_is_retransmitted_after_rto` 绿——丢数据段 → RTO 内不重传 → 拨钟越 700ms → 重传 → 对端收齐。§2 五条不变量达成（例外见下"范围修订"）。

**范围修订（对 §3-S4 的诚实偏离）**：实施中查实 `rx_datagrams`/`tx_datagrams` **同时服务外部 UDP 路径**（`step_device_tx.rs` 有完整 UDP 车道 + ARP 解析；外部 RX 经 `record_recv_payload` 落队列），而外部真网卡 UDP 目前是通的，P1 不能碰坏 → S4 收窄为"杀直拷、单通路转运，UDP 队列保留为唯一缓冲（smoltcp udp::Socket 本就不持数据，无双份）"；**"smoltcp 接管 UDP 数据"随 P2 外部统一时一并做**（单次重写优于两次）。

**S4 发现并修复的 SMP 竞态**（重要副产物）：`-smp 4` 下 UDP smoke ~1/3 概率内核 panic（`cap.rs:349`，S3 基线 6/6 绿）。机理：队列转运使 poll 路径高频触碰可能被并发 close 退休的外来 socket `Cap`，而 **`Cap` 裸解引用/clone 对已退休槽是 panic**（`cap.rs:349/280`）。修复 = `poll_udp_egress/ingress` 与 `NetworkPublishTarget::publish` 改经 `downgrade().observe(guard)` 检活取 `IdentRef`（guard 钉内存 + 无 Cap deref）；`publish` 内取 guard 必须用 `borrow_current_guard()`——**EBR 禁嵌套 guard，直接 `guard()` 实测 6/6 必炸**（`epoch/mod.rs:59-61` 自述）。修后 smp4 8/8 绿。**遗留**：同形 `Cap` 裸 deref 遍布 net 代码（TCP poll 路径同样有此暴露），系统性加固归 P3；用户告知本机多核环境本就不稳，后续验证以单核为准。

**验证总账**：host 套件失败集合与 P0 基线逐条相同（305 既有 + 新灵魂测试全量挂于同族毒化级联、单跑绿）；UDP loopback 测试族单跑 10/10；QEMU 冒烟——S4 修复后 smp4 UDP 8/8 + TCP 3/3，S5 后单核 tcp/udp 各 2/2。**未做**：LTP 全量对照（本机无 sdcard 镜像，`recv01/recvfrom01` 单跑以 cold-start smoke 为代理）——下次接触 LTP 环境时按 §4-3 补验。

---

## 附录 A. 证据锚点（三路调查汇总）

<!-- txdoc:07-NET-P1-V1-EVIDENCE -->

**通路选择与直拷（`@5ab58517`）**：`step_send.rs:123-124/266-267/363-370/389-426`；`payload.rs:634-636`；`tcp.rs:127-134`。
**段级路径**：`step_tcp_loopback.rs:40-133`（入口）、`:135-252`（transfer，平账 :227、手工流控 :181-187、冻结时戳 :190/:319）、`:302-352`（真握手）；`poll_context.rs:57-76`（egress=dispatch_segment）、`:489-532`（ingress=process_segment+staging搬运）、`:612-650`（accept 晋升）。
**外部旁路**：`step_process_network_events.rs:275-295`（裸塞+猜测性 ack :287）、`:298-306`（手工 child）；`smoltcp_demux.rs:41,57`。
**缓冲触点全表**：`tcp.rs` —— rx_buffer :98/119-123/127-134/139/147-158/166-183/384；tx_buffer :99/187/204/264-266/296-298/311-315/326-336/385；corked_tx :100/188/235-262/275-300/519-528；last_syn_ack :97/440/446/531-534；protocol_state :96/359/390/424/462-484；smoltcp ring 尺寸 :538-545；`send_slice` 唯一点 :511-512。外部调用：`payload.rs:626/634/643/650/659/725-740/834/861-912/973/1177/1224/1256`；`step_recv.rs:42/48/91/102-107`；`step_socket_close.rs:174/185/192/215`；`step_tcp_backlog_poll.rs:60-61`；`step_device_tx.rs:209`；`tx-shims helpers.rs:313`、`socket.rs:2185-2213/3051-3067`。
**UDP**：`udp.rs:20-29/75-77/99-161/186-269/279-292/296-352`；`step_udp_loopback.rs:24-99/101-219`；`poll_context.rs:78-105/107-186/310-358`。
**就绪/唤醒**：`readiness.rs:3-34/45-67`；`identity.rs:18-25/90-98`；`wait_source.rs:71-97`；`execution/mod.rs:101-131`；`publish.rs:27-48`；`tx-substrate/bus/queue.rs:215-249`；`payload.rs:1295-1333`。
**设备/队列**：`loopback.rs:9/19-22/52/85-99`；`device.rs:64-84`；`step_loopback_pending.rs:15-32/90-243`；`delegate/runtime.rs:228-245`。

**关联**：[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md)（§3 D1/D6/D7/D8、§5 P1）、[`REFACTOR_P0_v1.md`](REFACTOR_P0_v1.md)（前置：时钟解冻）、[`NET_AUDIT_v1.md`](NET_AUDIT_v1.md)（①②③ 病根、R2d 内存）。

---

*P1 完成后：loopback 上只剩一条 smoltcp 段级通路，数据单一居所，重传/背压/FIN 全归状态机——P2（外部网卡走同一 poll）在此骨架上把 eth0 接进来即可。所有 `file:line` 按 `5ab58517`。*
