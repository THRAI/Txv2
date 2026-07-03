# 网络栈重构 P2 执行计划 v1：外部网卡走同一 poll，打通外部 TCP/UDP

<!-- txdoc:07-NET-P2-V1 -->

**Status.** v1 (2026-07-02)。[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md) 阶段 **P2** 的可执行细化。前置：P0（时钟桥，`5ab58517`）、P1（loopback 单通路，`2fc0a3ea…402a441d`，见 [`REFACTOR_P1_v1.md`](REFACTOR_P1_v1.md) §8）。§6 有 **4 个设计点待拍板**；分步骨架（§3 S0–S7）已细化。

**Purpose.** 修审计 **①（外部 TCP 结构性不可用）**、**③（外部 UDP 收敛）**、**R3a（RX 校验和，P1 已覆盖 demux 入口）**，并完成 P1 遗留的"UDP 数据进 smoltcp"。目标可用一句话验收：**guest 里 `busybox wget http://10.0.2.2:8000/marker` 返回 rc=0**。

**基准.** 工作树 `feature-network-refactor @ cc69f5d2`。三路证据：外部 TX/路由调查、外部 RX/connect 断点调查、`net-git` 分支闯关实录（该分支在旧架构上把外部 TCP 从零打通的完整 commit 序列——P2 的"参考答案"）。

**给现场赛的读者.** §1.2 的"八关对照表"是本文的灵魂：前人踩过的每一个坑都有 commit 可查，我们的新架构哪些坑已结构性消失、哪些还要打，一张表看完。

---

## 0. 一句话

<!-- txdoc:07-NET-P2-V1-ONELINE -->

> 外部路径的**四肢健全、心跳缺失**：TX 车道（组帧/ARP/virtio）齐备且每轮都在空转等包，RX 链（virtio→demux→带完整段的事件）在 P1 后也通到了 `process_segment`——但**没人对外部 socket 调 `connect_endpoint`**（出站 SYN 无源头），**入站 SYN 被"手工造 Connected 假 child"劫持**（smoltcp 停在 Closed，握手/数据全假）。P2 = 接上这两处心跳（出站真 connect + 入站真握手），再按 net-git 实录清完周边七关（ARP/RX 驱动/EPIPE/多段 TX/端口轮转/UDP/驱动 MMIO）。

---

## 1. 现状全景

<!-- txdoc:07-NET-P2-V1-MAP -->

### 1.1 外部路径解剖（四条链，两断两通）

**✅ TX 车道（通，空转待料）**——纯 delegate 轮询，syscall 侧只 `kick_poll`：

```
delegate 每步 → step_process_device_tx_pending（step_device_tx.rs:118-189）
  按预算扫四类 socket（tcp_connecting=16 / tcp_connected=32 / udp=32 / raw_icmp=32）
    TCP: raw_tcp.dispatch_segment() → emit_ipv4_packet（裸IP包）        ← :208-232
    UDP: peek_udp_tx_datagram → emit_ipv4_packet（peek-then-commit）    ← :234-282
  → sink.transmit_at → EtherIface::dispatch_ip_at（ether.rs:283）
       decide_ipv4_route（:1218 同子网直连/否则网关）→ ARP 解析 → 组以太帧 → ops.transmit → virtio
```

关键事实：**`is_tcp_connecting` 车道已存在**（:133）——每轮对 Connecting socket 调 `dispatch_segment`。只要 smoltcp socket 真的进了 SynSent，SYN 会被自动送出网卡。ARP 机制完整（缓存 300s/挂起/1s×3 重试/学习，ether.rs:634-718）；未解析时 IP 包丢弃靠 TCP 重传兜底——**P0 之前重传是死的，这条设计等于断路；P0 之后它第一次成立**。

**✅ RX 链（通至状态机门口）**：virtio `ops.receive` → `NamespaceEtherPacketSource::next_packet_at`（namespace.rs:1779，含跨 ns 转发检查）→ `process_frame_at`（ether.rs:229，以太解析/MAC 过滤/分片重组/ARP 学习/ICMP echo 应答）→ `demux_rx_frame_with_smoltcp`（P1 后带完整段+校验和验证）→ `step_process_network_events` → established 命中则 `process_segment`（P1-S1）。

**❌ 断点一：主动 connect 不发 SYN**（证据链全）：`step_connect.rs:103` 对 TCP 置 `Connecting` 后，`try_tcp_local_namespace_connect`（:121）只处理"本内核内某 ns 拥有的地址"（手工造 child）；真外部地址 → None → `yield_on_token` 永久等待。**全仓只有 loopback 握手（step_tcp_loopback.rs:318）调过 `connect_endpoint`**——外部 Connecting socket 的 smoltcp 永远停在 Closed，`dispatch_segment` 永远 None，TX 车道空转。

**❌ 断点二：入站 SYN 走假握手**（step_process_network_events.rs:305-319）：查到 listener 后 `create_connected_stream_for_accept_in_namespace` 手工造 child——smoltcp **Closed**、协议枚举直接标 `Connected`（registry.rs:121 只改枚举）、**跳过半开 backlog 直接进 accept 队列**、SYN-ACK 不发、ACK 不等。对真实外部客户端不可用（收不到 SYN-ACK）。**连锁**：P1-S1 的 established-RX 对这种 child 无效——smoltcp 是 Closed，`accepts()` 拒收，数据静默丢弃。而**真握手机制已经存在**（`process_first_syn`，poll_context.rs:344-388：child 真 `listen_endpoint` → 喂 SYN → SYN-ACK → 半开 backlog → ACK 后晋升），只是**只挂在 LoopbackIface 上**。

**周边事实**：双路由并存（真发包用 iface 内嵌 `decide_ipv4_route`+硬编码网关 init/net.rs:35-37；syscall 侧/转发用 namespace FIB，且 boot 不写默认路由进 FIB）；net IRQ 未注册（irq.rs 只有 UART），RX 全靠 poll+kick；IPv6 以太帧在 demux 入口整帧丢弃（smoltcp_demux.rs:17-22、ether.rs:279）**而下层解析其实已支持 v6**；设备层双实现（staging 软件模型 + 真 virtio 驱动，boot 注册真驱动但 MMIO 被 blk 抢占 → 启动日志 `devices:net:init-skip:mmio`）。

### 1.2 net-git 八关实录 × 新架构对照表（本文灵魂）

`net-git` 分支在**旧架构**上把外部 TCP 打通的完整闯关史（每关一个 commit，可 `git show` 复查）：

| # | 关卡（旧架构的坑）                                                                          | net-git 解法                                                                                     | 我们的新架构                                                                                                                    |
| - | ------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------- |
| 1 | virtio0 被 blk 抢占，eth0 注册不上                                                          | `51fc5e5b` HAL 加 virtio1 @0x10002000（2 文件）                                                | **要做**（S0，可直接借）                                                                                                  |
| 2 | 外部 connect 不发 SYN                                                                       | `6e63bd29` try_tcp_external_connect：`connect_endpoint` + 注册连接 + demux 带段喂 Connecting | **要做**（S1）；demux 带段部分 P1-S1 已做                                                                                 |
| 3 | connect 阻塞期间无人收 SYN-ACK                                                              | `c8389512` RX drive window（10ms clamp+deadline 发 POLL）                                      | **要做**（S2）                                                                                                            |
| 4 | ARP 学习断层（回应学进别的 iface）+ RX 不推进                                               | `44d7895c` 静态网关 ARP + poll-pump task（有节制不饿死用户态）                                 | **要做**（S1 静态 ARP + S2 pump）                                                                                         |
| 5 | 握手完成但阻塞 connect 不续跑（wake 后 syscall 载体不重跑）                                 | 同上 commit：RawQueueWaitFuture 电平 peek fallback                                               | **风险项**（S2 验证；本质是⑥/D14 族 wait 坑，可能前置 P3 小块）                                                          |
| 6 | 每个 Connected 都被假设有内核内对端 → 外部首写 EPIPE                                       | `3b7bfe26` 无内核对端时以 `may_send` 判 EPIPE                                                | **要做**（S1；`tcp_connected_peer_error` 在本分支原样存在 step_send.rs）                                                |
| 7 | 顺序连接撞死：端口固定起扫 + 一次性 context ISN 恒同                                        | `9d840ecb` 端口轮转 + RNG nonce                                                                | **半结构性解决**：持久 `CONTEXT_IFACE`（P0）使 rand 持续前进 → ISN 已异；connect-autobind 端口轮转**要做**（S5） |
| 8 | 多段 TX：每轮只发一段，>MSS 的 TLS ClientHello 卡死                                         | `9c919782` device_tx 每轮抽干可发段                                                            | **要做**（S4；本分支 process_tcp_tx_socket 同款单段）                                                                     |
| + | UDP/DNS egress 三小坑：DNS 10.0.2.3 无 ARP、外部 UDP 发送不开驱动窗、loopback step 吞外部报 | `9c919782` 后半                                                                                | 第三坑 P1-S4 已有 dst 过滤；前两坑**要做**（S6）                                                                          |
| + | 时钟冻结                                                                                    | `9c919782` NET_NOW_MICROS 补丁                                                                 | ✅**P0 结构性解决**                                                                                                       |
| + | established 外部连接的 RX 喂养                                                              | `9c919782` process_tcp_event_external_connected                                                | ✅**P1-S1 结构性解决**（所有 established 统一喂 process_segment）                                                         |

> 借用纪律：net-git 的 commit 是**旧架构上的补丁**，思路可借、代码需按新架构重写（那边是一次性 context/RX 旁路世界）；唯 #1（HAL）与测试文件（`external_connect_tests.rs`，326 行）接近可直接移植。

### 1.3 可复用的地基（P2 不需要新建的东西）

TX 车道全套（含 Connecting 搬运）、ARP 全套、demux 带段+校验和（P1）、真握手机制 `process_first_syn`（只差 iface 泛化）、established RX 统一喂养（P1）、时钟（P0）、`peek-then-commit` 的 UDP 设备背压、fork 的 UDP socket API（bind/send_slice/recv/peek，P1 预检过）。

---

## 2. 目标形态

<!-- txdoc:07-NET-P2-V1-TARGET -->

```
                     出站 connect                          入站 SYN
改造前: Connecting → (无人调 connect_endpoint) → 永久 yield   手工造 Connected 假 child(smoltcp Closed)
改造后: Connecting → connect_endpoint 发 SYN ──┐             process_first_syn(泛化) → 真 child listen
                                               ▼               → SYN-ACK → 半开 backlog → ACK → 晋升
        device_tx 车道送 SYN 出网卡 ← (已存在,自动)          与 loopback 完全同构,只是包从网卡来

           RX: virtio → demux(带段) → established: process_segment(P1 已通) / SYN: 真握手(S3)
           TX: dispatch_segment 抽干循环(S4) → 路由/ARP/组帧(已存在) → virtio
           UDP: 数据进 smoltcp udp::Socket(S6),loopback 与外部同源,删自研双队列
```

验收不变量：

1. **出站三次握手在真网卡上完成**（pcap 可见 SYN/SYN-ACK/ACK），`connect()` 返回 0；
2. **`wget http://10.0.2.2:8000/marker` rc=0**（连接+多段收发+FIN 全链路）；
3. **DNS 解析通**（`nslookup` 经 10.0.2.3，UDP 出站+入站）；
4. **入站握手真实**：外部 SYN → 我们发 SYN-ACK → ACK 后才可 accept（半开 backlog 生效，含 SYN-ACK 重传）；
5. **loopback 全量不退化**（P1 的所有门继续绿）。

---

## 3. 分步实施（S0–S7，每步独立编译、可验证、可提交）

<!-- txdoc:07-NET-P2-V1-STEPS -->

> 步序按依赖排：先有设备（S0），再出站（S1-S2），再入站（S3），再吞吐/健壮（S4-S5），再 UDP/DNS（S6），扫尾（S7）。每步验证沿用 P1 方法论（host 集合差 + 单核 QEMU），新增外网验证环境见 §4。

### S0 —— 设备与验证环境就位

- 借 `51fc5e5b`：HAL 暴露 virtio1 @0x10002000，net 驱动改探 virtio1（blk 留 virtio0）。boards/tx-hal-riscv64-qemu-virt + devices.rs 两处小改。
- QEMU 冒烟命令加 `-device virtio-net-device,bus=virtio-mmio-bus.1 -netdev user,id=net0`（比照 oscomp.rs:300 的接法）。
- **验证**：boot 出现 `devices:net:eth0:ok`（替代 `init-skip:mmio`）且 busybox-boot/loopback 冒烟不退化；`ip addr` 可见 eth0 10.0.2.15。

### S1 —— 出站心跳：connect 发真 SYN + 周边三小修

- `step_connect` 外部分支（`try_tcp_local_namespace_connect` 返回 None 之后、yield 之前）：对裸 socket 调 `connect_endpoint(local, remote)`（借 `6e63bd29` 思路；我们的世界更简单——持久 `CONTEXT_IFACE`+活时钟已就位）→ smoltcp 进 SynSent → **device_tx 既有 Connecting 车道自动把 SYN 送出**。连接注册进表（RX 回包按四元组命中 established 分支 → P1-S1 的 process_segment 通用喂养把 SYN-ACK 喂进去 → Connecting→Connected 晋升复用 `promote` 语义，注意出站客户端没有 listener/backlog，晋升 = 改协议枚举 + fire SPACE）。
- 静态网关 ARP（借 `44d7895c`）：boot 时给 EtherIface 灌 10.0.2.2 的静态表项（学习断层修复归 P4/D10，先兜底）。
- EPIPE 修复（借 `3b7bfe26`）：`tcp_connected_peer_error`（step_send.rs）无内核内对端时改按 `may_send` 判定。
- **判决性单测（借 net-git 的 `external_connect_tests.rs` 移植）**：connect → 断言 SynSent + dispatch 产出 SYN → 注入 SYN-ACK 段 → 断言 Established + Connected + SPACE 唤醒。**这是 P2 的灵魂测试**——不需要真网卡就锁死出站握手逻辑。

### S2 —— RX 驱动：连接期间让包进得来

- 外部 connect/发送打开**驱动窗口**（借 `c8389512`+`44d7895c`）：delegate 下次醒来钳到 ~10ms、deadline 到点发 POLL（不只 TICK）；或按 net-git 最终形态起有节制的 poll-pump（不饿死用户态）。设计点 §6-4：pump vs IRQ。
- **专项验证坑 5（阻塞 connect 续跑）**：握手完成 wake 之后，阻塞的 connect syscall 必须重跑 step_connect 观察到 Connected。net-git 在这里栽过（wake 发了、载体不动）。若复现：最小修复 = wait 载体的电平 peek fallback（借 `44d7895c` 该 hunk），并在 P3/D14 记账。
- **验证**：QEMU + user-net 里 `nc 10.0.2.2 8000`/自写 smoke 完成三次握手（pcap 佐证）；阻塞与 O_NONBLOCK 两种 connect 都必须测。

### S3 —— 入站心跳：真握手取代假 child ✅（2026-07-03 完成）

> **实施记录**：外部 SYN 分支重写为真握手三分支（表命中喂段 / 首 SYN 建半开 child + `listen_endpoint` + 喂 SYN / 半开兜底喂段），与 loopback 共享 `promote_connected_stream_and_publish_accept`（晋升时才入连接表，loopback 对称）；SYN-ACK 初发+RTO 重传走 device-TX 新增**半开车道**（遍历 listener connecting backlog 调 `dispatch_segment`，smoltcp 自身门控，无 deadline 簿记）——**§6-1 的 iface trait 未再需要**（P1 后握手机制已 iface 无关，出口天然复用 sink）。假握手包装函数删除。镜像灵魂测试重写 accept_poll_tests（SYN→断言半开+SYN-ACK ack 号→ACK→断言晋升 accept）。**验收**：hostfwd 宿主 `nc` 真机三跑全过（SYN→SYN-ACK→ACK→双向数据，pcap 佐证）；回归 308=308 集合全同 + 出站/loopback/busybox-boot 全绿。
> **⚠️ 附带发现（§5-1 悬案证实）**：QEMU virtio-mmio 冷空闲态 RX **帧进缓冲但不举中断线**（探针矩阵：pump-rx-ready=1 / extirq=0），即 net-git stage3 悬案本尊。按 §6-4 预案落**混合形态**：IRQ（活跃流低延迟，实测有效）+ 反应器 WFI 空闲拍 ~5ms 低频 pump 兜底（exec.rs，零热路径开销）。纯 IRQ 之谜（疑 EVENT_IDX/QEMU 设备模型）单独立项再攻。

- `process_tcp_event` 的 SYN 分支（events.rs:305）废弃 `create_connected_stream_for_accept*`，改走 `process_first_syn` 的真握手（child `listen_endpoint` → 喂 SYN → 产 SYN-ACK → 半开 backlog → ACK 到达经 established 分支 process_segment → connected 边沿晋升 accept 队列）。
- 结构前提：`process_first_syn`/`PollContext` 目前吃 `&LoopbackIface`（SYN-ACK 回程要 `iface.dispatch_ip`）。泛化方式见设计点 §6-1（推荐抽 IfaceTx 小 trait：loopback=入队，ether=dispatch_ip_at）。
- backlog 的 SYN-ACK 重传（P1-S3 已归 smoltcp 定时器）对外部自动生效——但重传段的**回程发送**同样需要 iface 泛化。
- **验证**：宿主 `nc 10.0.2.15` 不可行（slirp 无入站），用 hostfwd：`-netdev user,...,hostfwd=tcp::7777-:7777`，宿主 `nc 127.0.0.1 7777` 连 guest listener——三次握手+数据往返；同时 loopback accept 族测试不退化。

### S4 —— 多段 TX：抽干循环

- `process_tcp_tx_socket`（step_device_tx.rs:208）从"每轮一段"改为循环 `dispatch_segment` 直到 None 或 sink Busy（借 `9c919782` 第一修；注意保留 Busy 背压与预算，别把 delegate 一轮跑成无界）。
- **验证**：单测——发 >2×MSS 数据，一轮 device_tx 后 smoltcp send_queue 清空/段计数≥2；QEMU wget 一个 >4KB 文件。

### S5 —— 顺序连接健壮性

- connect-autobind（tx-shims `maybe_autobind_connect_client`）临时端口改走共享轮转（借 `9d840ecb` 前半，对齐 `bind_with_ephemeral_port`）。
- **验证 ISN 已异**（持久 iface 的 rand 前进）：单测两次 connect 的 ISN 不同；QEMU 连续 4 次 wget 全成。

### S6 —— UDP 收敛进 smoltcp（完成 P1 遗留）+ DNS

- `RawUdpSocket`：bind 时同步 `socket.bind`；send 走 `send_slice(payload, meta)`；recv 走 `recv/peek`（src 从 `UdpMetadata` 取）；**删 `rx_datagrams`/`tx_datagrams`**——此时两类用户（loopback 转运 + 外部 device_tx/RX）一起换源：loopback egress/ingress 与 device_tx 的 UDP 车道改为 `socket.dispatch/process`；外部 RX 的 `record_recv_payload` UDP 分支改喂 `process`。
- DNS 两小修（借 `9c919782` 后半）：10.0.2.3 静态 ARP；外部 UDP 发送打开驱动窗口。
- **验证**：UDP 回环测试族 10/10 不退化；`nslookup example.com 10.0.2.3` 返回记录；`udp-loopback-smoke` 绿。

### S7 —— 扫尾

- IPv6 demux 放行（设计点 §6-2 若拍 A）：以太入口 `EthernetProtocol::Ipv6` 走 v6 解析（下层已支持），NDISC 仍静态表；ping6/路由归 P4。
- `Cap` 裸 deref 加固**先行小块**：外部 RX/device_tx 高频触碰外来 socket（P1-S4 竞态教训同族），把 events/device_tx 的 `acquire_operational` 入口换 `observe(guard)` 模式（全面收敛仍归 P3）。
- 死代码清扫：`create_connected_stream_for_accept*` 假握手族、双路由不一致记账（FIB 默认路由补写或明确注释归 P4）。

---

## 4. 测试方案

<!-- txdoc:07-NET-P2-V1-TEST -->

1. **判决性单测（不需要网卡）**：S1 的移植版 `external_connect_tests`——connect→SYN→注入 SYN-ACK→Established；S3 的镜像版——注入 SYN→断言我们产出 SYN-ACK+半开条目→注入 ACK→断言晋升 accept。两条合起来锁死双向握手逻辑，与 loopback 灵魂测试同级。
2. **QEMU 外网矩阵（单核，按用户要求）**：宿主起 `python3 -m http.server 8000`（slirp 把 guest→10.0.2.2:8000 转到宿主 loopback）：
   - 出站：`wget -O- http://10.0.2.2:8000/marker` rc=0（S2 后基本形态，S4 后大文件）；
   - DNS：`nslookup` 经 10.0.2.3（S6）；
   - 入站：hostfwd + 宿主 `nc`（S3）；
   - 顺序：连续 4 次 wget（S5）。
3. **不退化门**：host 套件失败集合对照（305 基线法）；loopback tcp/udp 冒烟；P1 灵魂测试。
4. **pcap 佐证**：QEMU `-object filter-dump,id=d0,netdev=net0,file=…` 抓包核对握手/重传（net-git 全程靠它定位，现场赛值得学会）。
5. **暂缺**：LTP 全量与 net_stress（本机无镜像）；`-smp 4` 并发（用户环境多核不稳）——两项都在有环境时补，P2 验收以上述矩阵为准。

---

## 5. 风险与已知坑

<!-- txdoc:07-NET-P2-V1-RISKS -->

1. **virtio-net RX 描述符疑案**：net-git stage3 曾遇"缓冲已投递+notify、QEMU 就是不完成 RX 描述符"，stage4 靠 poll-pump+静态 ARP 绕过但**真因未曾定论**——P2 的 S0/S2 若复现同症状，优先对照 net-git 的 `rx_primed_count`/VirtioNetStats 诊断路数，不要从头猜。
2. **坑 5（阻塞 connect 不续跑）是 wait 载体级问题**：属⑥/D14 族。若 S2 复现，只打最小 fallback 补丁并明确记账给 P3，不要在 P2 里顺手重构等待机制。
3. **`Cap` 裸 deref 竞态**（P1-S4 同族）：外部路径让 poll 触碰外来 socket 的频率再上一个量级；S7 的先行加固不可省。
4. **双路由不一致**：S1 起出站选路走 iface 内嵌路由（现状），FIB 只读——若 LTP route 族测试要求 FIB 生效，冲突暴露时按 D9 桥接原则处理，勿在 P2 重写路由。
5. **验证环境依赖宿主服务**：http.server/nc/hostfwd 都要在宿主起进程——脚本化进 `tools/`（见 §4），避免手工步骤漂移。
6. **回滚单元 = S 步**，同 P1；S3（换假握手）是行为面最大的一步，其 commit 必须可独立 revert 而不影响 S1/S2 的出站能力。

---

## 6. 待拍板设计点（4 个）

<!-- txdoc:07-NET-P2-V1-DECISIONS -->

| # | 问题                           | A（推荐）                                                                                                                                                                      | B                                                                                                                                                       |
| - | ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1 | 入站真握手怎么复用到外部 iface | **抽一个极小的"iface 回程发送"trait**（loopback=入队 `dispatch_ip`，ether=`dispatch_ip_at` 组帧发卡），`process_first_syn`/握手回程按 trait 走——一套机制两种后端 | 给外部路径复制一份握手代码（快但立即产生双份逻辑，违背 P1 刚建立的单通路原则）                                                                          |
| 2 | IPv6 范围                      | **P2 只放行 demux/ether 入口的 v6 帧**（下层解析 P1 已通，TCP/UDP v6 事件直接复活），NDISC 用静态表；ping6/邻居/路由全家归 P4                                            | v6 全推 P4（省 S7 半步，但 v6 数据路径继续全断，审计⑩多欠一期）                                                                                        |
| 3 | UDP 收敛时点                   | **P2 内做（S6）**——外部+loopback 两类用户此时同时在场，一次换源；再拖则 P3/P4 每期都背着双队列                                                                         | 再缓（若 S6 实施中发现设备车道耦合超预期，允许拆成独立后续，但须写明理由）                                                                              |
| 4 | RX 驱动形态                    | **poll-pump/驱动窗口**（net-git 已验证的形态，可控、不碰驱动）                                                                                                           | 接 virtio IRQ（架构上更正统——IRQ kick_poll 管道其实已在`ack_interrupt_and_fire`，缺的是 irq.rs 注册；但驱动层风险大，QEMU RX 疑案未破前不建议首选） |

> **拍板（2026-07-03）：1-A / 2-A / 3-A / 4-B（用户改选 IRQ 方案）**。4-B 执行纪律：S2 优先接 virtio IRQ（中断 handler 只 `kick_poll`，处理仍留在 delegate）；若撞上 net-git stage3 的 RX 描述符悬案且短期无法定位，**降级 A（poll-pump）保 P2 主线**，IRQ 单独立项再攻。

---

## 7. 深度调试根因（2026-07-03，纯 B 死磕结论）

<!-- txdoc:07-NET-P2-V1-ROOTCAUSE -->

**背景.** 4-B（IRQ）落地后，外部 TCP 三次握手在真 virtio-net 上完整完成（pcap 实测 SYN/SYN-ACK/ACK 稳定复现），但阻塞 `connect()` 之后 `wget` 不出 HTTP GET。用户要求纯 B 死磕，遂用内核 AtomicU32 计数器全链路插桩（`net_delegate_step_once`→`process_tcp_event`→`publish_to`→`step_connect`→`connect_impl`→`run_thread`→`enter_userspace_with_context`），throttled `console_write_str` dump。**插桩已全部撤除，工作树干净。**

**计数器实测（一次外部 connect）**：`es=1 bc=1 pr=1 wk=2`（收 1 个带段 SYN-ACK、smoltcp 到 Established、Connecting→Connected 晋升、fire SPACE **唤醒 1 个订阅者**）；`ce=2 sy=1 st=Bound1/Connected1`（step_connect 跑两次：首次 Bound→Connecting yield，二次见 Connected）；`ci=park1/woke1/eisc-ok1`（connect_impl park→**await 返回**→`Err(EISCONN) if waited_for_connect` 命中→**`return Return(0)` 执行**）；`as/be=1/1`（run_thread 存 `pending_syscall_return(Ok(0))`→**到达并调用 `enter_userspace_with_context`**）；**但 `tr=connect1/write0/nr=203`——线程从没 trap 到 write**。

**逐层判决**：
- 网络栈**全对**：握手/段处理/晋升/唤醒链无一环断（`wk=2` 证明 fire 确实唤醒了 parked connect 的订阅者，非丢唤醒）。
- syscall 层**全对**：`connect_impl` 收到唤醒、await 返回、走 EISCONN 分支、`return Return(0)`；`run_thread` 存返回值、调用 `enter_userspace_with_context`。
- **断点在 `enter_userspace_with_context` 的用户态往返**：它被调用了（`be=1`），但用户态 `write` 的 ecall 往返没回到 `run_thread`（`tr write=0`、`nr` 停在 203）。

**根因.** 一个**被跨任务事件唤醒**的阻塞 syscall（外部 connect 由 net IRQ→delegate→`fire_send(SPACE)` 完成）在 `run_thread` 重入用户态时，`enter_userspace_with_context`（`board trap.rs:813`→`tx_rv64_enter_userspace_save_resume`）的 per-hart reschedule-longjmp 往返**在"net 唤醒的 reactor poll 上下文"下不完整**——`sret` 进用户态后，`write` 的 trap 未经 `TrapAction::Reschedule` longjmp 回本次 poll 的 `enter_userspace` 调用点，线程不推进到下一条 syscall。这是**平台 trap-shell / 线程 future 重入的架构限制**，与网络栈无关。

**为什么 net-git 能过.** net-git 用 poll-pump（A）间接绕过：poll-pump 让 trap-shell reactor 外循环持续转，`run_thread` 的用户态重入由 trap-shell 自身上下文驱动，longjmp 目标有效。net-git stage5（`a1b7417d`）自述 "with poll-pump, blocking connect() completes"，正是此机制。本轮把它比 net-git 更深钉了一层（net-git 停在"connect succeeds, stuck in post-connect syscall"，未定位到 `enter_userspace` 往返）。

**追加实测（2026-07-03，pump 假设证伪）**：写了一个无条件 ~2ms `net_delegate_kick_poll` pump 任务做经验测试——**connect 仍不恢复**（pcap 仍停在 ACK、无 GET、无 ext-ok）。**证伪"poll-pump 能兜底 connect-resume"**（plan 4-A/4-B 降级假设）：pump 唤醒的是 delegate 任务，而 gap 在 run_thread 的用户态重入路径，两者正交。**推论**：4-A 与 4-B 在当前架构下**共享同一 connect-resume gap**，poll-pump 对它无效（pump 只解决 RX 驱动，而我们的 RX 已由 IRQ 解决）。pump 已撤除。

**net-git 为何能过（复核纠正）**：其 a1b7417d/3b7bfe26 几乎全是诊断，**无专门线程恢复修复**。真机制是 stage3（`c8389512`）的 **RX drive window 在 connect syscall 自己的上下文里同步驱动 RX**——SYN-ACK 在 connect 执行期间被处理、连接在 connect **自身 trap-shell 上下文**内 Established、connect 正常返回，**根本不 park、不经跨任务唤醒**，从源头绕开 enter_userspace 重入 bug。我们的实现让 connect `yield_on_token` park 在 send carrier、靠 delegate 跨任务唤醒完成，正撞此 bug。

**修复路径（更新后，待决策）**：
- **(A′) inline external-connect drive（正解，仿 loopback）**：给外部 connect 一个**同步驱动循环**（类比 `drive_tcp_loopback_after_connect`），在 connect syscall 自身上下文内驱动设备 RX/TX 直到 Established 或超时，使 connect **不 park**、从 trap-shell 上下文正常返回。障碍：设备驱动在 tx-kernel 的 boot delegate，connect 在 P-无关的 tx-subsystems / 有 P 的 tx-shims——需一个"同步 pump 当前 netns 设备一轮"的 P-having 入口（shim 层 `drive_tcp_loopback_after_connect` 的外部 analog）。**这是 net-git 的实际做法，工作量中等,层次是主要难点。**
- **(B) 架构修复（正统、大）**：让"跨任务唤醒的阻塞 syscall"的用户态重入 defer 回 trap-shell 上下文，需动 trap-vector/reactor/thread-future 核心执行模型，宜 gdbstub 单步 trap 汇编佐证后独立立项。
- ~~(A) 简单 poll-pump~~：**已证伪，不可行**（见上）。

### 7.1 终局改判（2026-07-03 三轮死磕）：真凶 = 两个可修 bug，"平台架构限制"说被证伪，A′ 不再必要

<!-- txdoc:07-NET-P2-V1-ROOTCAUSE-FINAL -->

上文"平台 trap-shell 重入架构限制"的判决**被更深一层的调查证伪**。新证据（探针 = `eu-ret`/`await-ok`/`extirq-wake` 三标记，`probe1.serial`）：**longjmp 其实回来了**——旧插桩只数了"出发"（`be=1`），没数"回程落点"；新探针显示第 4 次往返 `eu-ret` 打印（控制流回到 `enter_userspace_with_context` 调用点之后）但 `await-ok` 永不出现——线程 park 在 `entry_wait.await`（thread_future.rs:635）。用排除法收口：能触发 from-user longjmp 的所有 trap 路径中，syscall/缺页会 resolve slot、时钟会 `record_timer_preemption`，**唯一"longjmp 但不留记号"的是 `on_external_irq` 的 `Wake→Reschedule`**——且它恰在最后一次 `eu-ret` 前打印。

**真凶 ①（trap 纪律违反，tx-kernel/src/trap.rs `on_external_irq`）**：时钟中断打断用户态时先 `hand_off_timer_preempt`（存用户现场 + 给 userspace-run slot 记 Preempted）再返 `Reschedule`；外部设备中断路径**两样都没做**（连 trap frame 都拿不到），直接 `Reschedule`。板级 shell 对 from-user Reschedule 无条件 longjmp（board trap.rs:1051），`run_thread` 回来后 `entry_wait.await` 等一个**永远无人解决的 slot**——线程静默死亡。P2-S2 第一次打开 virtio-net 中断，"设备中断打在用户态时间片上"是内核史上首次发生的事件（此前该路径也威胁 UART：用户态时间片内敲键盘同样致死，只是从未被触发注意）。**修复**：`on_external_irq` 签名增 `TrapFrameMut`（HAL trait + rv64/la64 两板 + 3 测试桩），from-user 的 `Wake` 先走 `hand_off_timer_preempt` 同款纪律再 Reschedule。
**真凶 ②（virtio-net 驱动半拉子 NAPI，tx-drivers/src/virtio/net.rs）**：`ack_interrupt_and_fire` 在 rx_ready 时 `disable_interrupts()`（NAPI 式"忙时关中断"），但**全仓无任何重新开启点**（`enable_interrupts` 仅 boot 一次）——第一次网卡中断即把设备通知永久关闭，后续帧静默堆积（修①后：GET 发出、服务器响应 185+335+FIN 到网卡，但无中断→delegate 不 poll→不 ACK→read 不醒，服务器重传 6 次）。**修复**：删除抑制逻辑——PLIC 层 mask 窗口（顶半部 mask→底半部 ack+kick+unmask）已提供节流，设备层抑制冗余且缺另一半。
**旧判决为何错**：poll-pump 证伪实验时 IRQ 仍开着，真凶①照常杀线程，故 pump"无效"——正确结论应是"pump 治不了①"而非"跨任务重入不可修"；net-git 的 poll-pump 能过是因为**从未打开设备中断**，from-user 设备 IRQ 路径根本不存在。
**验收（2026-07-03，全实测）**：`tcp-external-smoke` **ext-ok ×4 稳定**，pcap 完整生命周期 SYN→SYN-ACK→ACK→GET→响应→FIN→**全 ACK 零重传**（35ms）；回归矩阵全绿——xtask unit 仅分支既有 ext4 失败（stash 对照）、tx-subsystems --lib 失败集合 308=308 完全一致、la64 构建过、busybox-boot(-smp 4) sentinel ok、loopback tcp/udp 冒烟 ok。**推论**：A′/B 决策作废——阻塞 connect/read 经 park→IRQ 唤醒→重入的正路已通，无需内联驱动；坑 5 关闭。

---

## 附录 A. 证据锚点（三路调查汇总）

<!-- txdoc:07-NET-P2-V1-EVIDENCE -->

**TX/路由（`@cc69f5d2`）**：`step_device_tx.rs:13-18/118-189/133/191-232/234-282/284-334/381`；`ether.rs:23-25/36-42/121-133/208/229/274/283-323/311-313/325/515-552/586-602/610/634-718/740/819-832/981-1002/1004-1021/1218-1230`；`init/net.rs:35-37/75-82/141-160/162-215/226-255/258`；`namespace.rs:73/226-236/755/974-1018/1770-1790/1816-1829/1915`；`delegate/runtime.rs:228/247-266/268-278/280`；staging 设备 `device/virtio.rs:17-35/115-117/229/277-343/353/375`；真驱动 `tx-drivers/src/virtio/net.rs:139/332-370/432/467/556-599`；`devices.rs:151/163/175`。
**RX/connect 断点**：`step_connect.rs:59/103/114/121/134/141/162/209`；`socket.rs:415/464-482`；`helpers.rs:3/21/188-201`；`step_tcp_loopback.rs:72/318`；`step_process_network_events.rs:88/119/282-290/305-319/333/367-372`；`registry.rs:77/92/121`；`poll_context.rs:344-388`；`table.rs:452/559/595`；`smoltcp_demux.rs:11/17-22/34/39/54`；`ether.rs:279/403`。
**net-git 实录**：`6e63bd29`（stage1 出站 connect+测试 326 行）、`51fc5e5b`（stage2 virtio1 MMIO）、`c8389512`（stage3 RX drive window）、`44d7895c`（stage4 poll-pump+静态 ARP+坑5）、`3b7bfe26`（stage6 EPIPE）、`7ce7c829/9d840ecb`（stage7 端口/ISN）、`9c919782`（多段 TX+UDP/DNS 五连修）、`2c97491b/e7992ef8`（fork/镜像，非网络，git 任务背景）。

**关联**：[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md)（§3 D5-D10、§5 P2）、[`REFACTOR_P1_v1.md`](REFACTOR_P1_v1.md)（§8 范围修订=S6 由来）、[`NET_AUDIT_v1.md`](NET_AUDIT_v1.md)（①③⑩ R3a）。

---

*P2 完成后：外部与 loopback 在"连接建立/数据收发/定时器"三个维度完全同构，netfilter/分层/IPv6 控制面（P4）与上半接入（P3）在此地基上继续。所有 `file:line` 按 `cc69f5d2`。*
