# PR #54 (`dd9435f3`) 网络侧审计 — 合并决策前置

日期 2026-07-27 · 审计对象 `dd9435f3`（main 上唯一改动 net 的 commit）· 目的：决定 `main → feature-network-refactor` 的合并策略

---

## 〇、结论摘要

`dd9435f3` "feat: integrate network time and reactor baseline"（PR #54 `codex/network-time-integration`，作者 3Y，2026-07-24）**不是一次网络增量提交**，而是：

> **把 `crates/tx-subsystems/src/net/` 大面积回退到 P0 解冻时钟之前的旧快照，再在旧基线上叠加 reactor/notification 的新工作。**

硬证据是 blob 级身份相等（不是"看起来像"，是逐字节同一个对象）：

| 文件 | main 的 blob | 与哪个历史快照逐字节相同 | 日期 |
|---|---|---|---|
| `net/protocol/tcp.rs` | `b3c7ddad` | `62158ead` / `858b5615`（P0 解冻前一版） | 2026-06-13 |
| `net/protocol/ether.rs` | `db692586` | `a3743b63`（P4-S4a 分层拆分之前） | 2026-06-02 |
| `net/packet/demux.rs`、`smoltcp_demux.rs`、`protocol/poll_context.rs` | — | `2fc0a3ea`（P1-S0 断直拷旁路之前） | 2026-07-02 |
| `net/protocol/udp.rs` | — | `baca7045`（P2-S6 DNS 打通之前） | 2026-07-02 |
| `net/netfilter.rs` | — | `167b9e2f` | 2026-07-03 |

共 **18 个 net 文件**在 main 上与 merge-base 之前的历史修订逐字节相同，且这些文件在 main 上**没有任何新增编辑** —— 纯粹的内容还原。任何独立演化路径都不会产生这种结果。

**决策含义：** MERGE_PROMPT.md 把"网络取 feature"写成**冲突时**的裁决规则，这是不够的。实际只有 **10 个** net 文件会真冲突，而 **约 60 个** net 文件 main 改了、feature 这轮没碰 —— git 会**静默取 main 侧**。按 prompt 字面执行能编译、能过大部分单测，但外部网络能力已经没了。

---

## 一、审计方法

```
merge-base b83a73d3 · main 8af194db · feature HEAD 90939012
```

- 先确认 main 侧 net 变更的唯一来源：`git log b83a73d3..main -- crates/tx-subsystems/src/net`
  → 只有 `dd9435f3` 和 `945b13c9`（后者是 PR#52 合并，其 net 树与 merge-base 一致）。
- `git diff 348559ec^2 348559ec -- net` 与 `git diff c9eba99d^2 c9eba99d -- net` **均为空** →
  证明 main 在 PR#53 时把 feature 的 net 树完整吃进去了，回退只可能发生在 `dd9435f3`。
- 判定"移走还是删掉"一律用 `git grep <ident> <rev> -- crates/` 全树反查，不看单文件。
- 判定"回退还是演化"用 `git rev-parse <rev>:<path>` 比对 blob 身份。

---

## 二、回退面

### 2.1 被删除的 7 个 net 文件

| 文件 | 判定 | 后果 |
|---|---|---|
| `net/clock.rs` | **DROPPED** | smoltcp socket 层时钟回到 `Instant::ZERO` 冻结：重传/RTO/RTT 估计/TIME-WAIT/keepalive 全失效。`CONTEXT_IFACE` 常驻 Interface 一并丢失 → 每次 connect 新建一次性 iface → **ISN 复用** |
| `net/tests/clock_tests.rs` | **DROPPED** | 唯一能红/绿区分"时钟是否解冻"的回归网消失。`tcp_loopback_lost_data_segment_is_retransmitted_after_rto` 也被连带删除 |
| `net/file_ops.rs` | **DROPPED**（连 `FileOps` trait 本身在 main 全树 **0 命中**，merge-base 10 个文件命中） | socket 退回非通用文件对象：VFS `step_read/step_write` 对 socket 返回 `EINVAL`，改由 `io.rs:1905/2265` 的 syscall 特判转 `sys_sendto/sys_recvfrom`。走 VFS 通路的调用者（sendfile/splice/通用 fd 抽象）对 socket 失效 |
| `net/adapter.rs` | **DROPPED**（main 上 20 个子系统都有 `adapter.rs`，**只有 net 没有**） | socket wait carrier 只进 subsystems registry、不进 substrate registry → 审计 R4a 的洞重新打开：纯 socket 集合的 `epoll_wait` 立即返回 0 而不阻塞。**✅ 已实证，见第 2.4 节** |
| `net/protocol/ether/l3.rs` | **MOVED 但弱化** → `protocol/ether.rs:759/781/832/912` | 4 个函数名都在，但丢了 `expire_and_cap_ipv4_fragments` / `last_seen` TTL 老化与 LRU 驱逐，改成到上限就 `fragments.clear()` 整表清空 → 可被分片放大成 DoS |
| `net/protocol/ether/link.rs` | **ARP 半边 MOVED；IPv6 动态 NDP 半边 DROPPED** | 只剩 `install_static_ndisc` / `remove_static_ndisc` 静态表。NS/NA 收发、solicited-node multicast MAC 放行、邻居重试全没了 → 外部 IPv6 连通不可用 |
| `net/tests/external_connect_tests.rs` | **DROPPED**（5 个 test） | 失去外部 TCP 三次握手、一次 pass 排多段、连续 connect ISN 唯一、外部 UDP sendto 到线的全部覆盖。其中 `sequential_connects_use_distinct_isns` 在 main 上**必然红** |

### 2.2 能力对照表（改写的 68 个文件）

| 能力 | merge-base | main | 判定 |
|---|---|---|---|
| **外部（非 loopback）TCP connect** | `step_connect.rs:139 try_tcp_external_connect()`；`step_process_network_events.rs` 用 `feed_tcp_segment()` → `raw.process_segment()` 喂已建立外部连接的 ACK | `git grep -c external main -- step_connect.rs` → **0**。`tcp_connected_peer_error` 直接 `return Some(Errno::EPIPE)` | **消失** |
| **外部 UDP + DNS** | `udp.rs` 667 行，`UdpInnerState{ socket: Box<udp::Socket>, ... }`，注释写明"smoltcp socket IS the data path" | `udp.rs` 396 行，退回 `rx_datagrams/tx_datagrams` 双影子 `VecDeque`。blob == `baca7045`，即 `c68eb4d2`「P2-S6 UDP 收敛进 smoltcp + **DNS 打通**」之前。全树无任何 DNS 相关代码 | **消失** |
| **单锁 enum `SocketImpl`（P3-B）** | `payload.rs:39 enum SocketImpl { Tcp/Udp/Icmp/Unix/Rds/Sctp/Packet/NetlinkRoute/NetlinkNetfilter }` | 九个并列 `Option<RawX>` + 独立 `io: SpinMutex<SocketIoState>`。`tcp.rs` 的 `SpinMutex` 计数 4 → **13** | **消失** |
| **RX 校验和验证（R3a）** | `smoltcp_demux.rs:93 ipv4.verify_checksum()`，TCP/UDP 全段校验 | `git grep -in checksum main -- net/packet/` → **空**（merge-base 8 处）。`smoltcp_demux.rs` 165 → 75 行，只 `new_checked` 后 `payload().to_vec()` | **消失** |
| **分片重组 LRU + TTL（R3b）** | 表 + `IPV4_REASSEMBLY_TTL=30s` + `expire_and_cap_ipv4_fragments()` + 4 个边界测试 | 只剩容量上限，满了 `fragments.clear()` | **消失** |
| **smoltcp 集成深度** | `CONTEXT_IFACE` 持久 Interface + `cx.now = net_now_instant()` 注入 | `with_context` 每次 `Loopback::new` + `Interface::new(..., Instant::ZERO)` 抛弃式新建，无时钟注入 | **退回 wire codec** |
| **ether 分层** | `mod.rs/link.rs/l3.rs` 共 1923 行，含 IPv6 V1/V2/V3b 全栈 | 单文件 1239 行，与拆分前只差 +7/−141 | **回滚** |

### 2.3 其他连带回退（未点名文件）

- **IPv6 数据面整体消失**：`namespace.rs` 3013→2681 行，`route6_snapshot / add_ipv6_route / delete_ipv6_route / best_ipv6_route / oif6_for_gateway / ipv6_prefix_matches` 等全部消失；`smoltcp_demux.rs` 的 `demux_ipv6 / demux_tcp_v6` 被删，main 对 `EthernetProtocol::Ipv6` 直接返回 `Unsupported`；`icmp.rs` 删 `enqueue_tx6_echo` 族。对应回退掉 `a90988d0`/`ccae95c8`/`28a05d02`/`14011f3b`（后者标题写着「QEMU 外部 ping6 3/3 全通」）。
- **`ip -6 route` 不可用**：`rtnetlink.rs` −216 行，`build_route6_message / parse_route6_config / ipv6_attr` 全消失。
- **conntrack 无界内存增长**：`netfilter.rs` −134 行，`expire_and_cap_masquerade()` / `expire_and_cap_dnat()` 及 3 个单测消失。
- **P1「TCP 直拷旁路」整体回归**：`step_process_network_events.rs:318/398` 用 `record_recv_payload` 把线上字节直接抄进 socket 缓冲，绕开 smoltcp 状态机 → seq/ack/window/校验和不参与判定，乱序与重复段被当正常数据交付。

**被撤销的重构提交清单**（至少）：P1-S0/S2/S3、P2-S1、P2-S6、P2-S7、P3-B S1/S2、P4-S2、P4-S4a，以及 IPv6 V1/V2/V3b、R3a、R3b。

### 2.4 R4a / epoll-on-socket —— 实证（2026-07-27）

原审计只做了静态推断，现已用**运行测试 + 分支对照**坐实。探针见 `.v6work/epoll-socket-r4a-probe.patch`
（`epoll_dispatch.rs` 里加一个 socket 版孪生测试，与既有的
`dispatch_epoll_pwait_blocks_until_eventfd_becomes_readable` 同形）。

测试内容：`socket(AF_INET, SOCK_DGRAM)` → `bind(127.0.0.1:24601)` →
`epoll_ctl(ADD, EPOLLIN)` → `epoll_pwait(timeout = -1)`，断言必须 `Poll::Pending`（停泊）。

| 分支 | 结果 |
|---|---|
| `feature-network-refactor` (90939012) | **PASS** —— 正确停泊 |
| `main` (8af194db) | **FAIL** —— `got Ready(Return(0))`，无限超时下立即返回 0 |

同一次运行里 eventfd 孪生测试两边都 PASS，排除了测试环境因素。

**故障链（main）：**

```
identity.rs:93  wait_source::register_wait_queue(recv_wq)
                  └─ 只 insert 进 subsystems REGISTRY（RawQueue 变体）
                     不调 tx_substrate::wake::register_source
fd_ready.rs:353 report.push_wait(token.source_id())
                  └─ FdWait{ endpoint: None }
                     ★ 这是 fd_ready.rs 里唯一不带 endpoint 的分支；
                       eventfd/pipe/socketpair/timerfd/signalfd/posix_mq/epoll 全走 push_endpoint
epoll.rs:161    epoll_wait_source() → (source, report.primary_endpoint() = None)
wait.rs:126-132 await_any_wait_source:
                  endpoint 为 None → lookup_source(source)   ← tx_substrate 的 registry
                                    → None → continue
wait.rs:141     active.is_empty() → return false
epoll.rs:528    if !wait_for_epoll_wake(..) { return SyscallResult::Return(0) }
```

`lookup_source` 实为 `tx-substrate/src/wake/wait_source.rs:584`，查的是 substrate registry；
而 main 上唯一往那里注册的是 `wait_source.rs:94 register_wait_source_with_id`，socket 走的
`register_wait_queue` 不走这条。**注意**：即使退一步查 subsystems 侧的 `lookup_wait_source`
也没用 —— 它只匹配 `RegisteredWaitSource::WaitSource` 变体，对 `RawQueue` 返回 `None`。

feature 侧之所以对，是因为 `identity.rs::register` 除了 `register_wait_queue_with_id`
还额外调了 `readiness.install_substrate_mirrors(wait_routing::new_wait_source(recv), ...)`
（`net/adapter.rs` 提供），把 recv/send/accept 三个载体镜像进 substrate registry。

**两个副发现：**

1. **`epoll_ctl(ADD)` 在 main 上是成功的** —— socket 被当作可 epoll 监听的 fd 接受，
   只是之后永不阻塞。属于静默失败，用户态看到的是 epoll 忙轮询而不是报错。
2. **未 bind 的裸 socket 两边都返回 0**（首轮探针就踩到这个，一度误判成"feature 也坏"）。
   原因是没 bind 就没有活的协议引擎/载体，`socket_poll_wait_token_from_file` 拿不到 token，
   在 `sources.is_empty()` 处就提前返回了 —— 与 R4a 无关，是另一个独立问题。
   **做这类探针必须先 bind**，否则结论会反。

---

## 三、真新增面 —— 合并 main 能拿到什么

`dd9435f3` 的增量是 **reactor / 时间基线**，与被删的网络能力**正交**：

- **`net/notification.rs`**（+42 行，唯一新增 net 文件）—— 不是网络功能，是"notification 汇聚屋"，把 `WaitToken` 降级为 reactor yield 形状。存在原因是 `xtask/src/lint_invariants_notification.rs` 的 `MAX_NOTIFICATION_BOUNDARY_VIOLATIONS = 0`：只有 `*/adapter.rs` 和 `*/notification.rs` 允许直接碰 `wait_source::*` / `YieldShape::OnWaitSource`。aio/eventfd/futex/io_uring/ipc 都已有，dd9435f3 把 net 也拉进这个约定。
- **network time 的真实含义**：net 内部全局时钟桥被删，时间改为 `now: Instant` 显式参数下传（`*_at` / `*_in_namespace_at` 系列）。新增 crate **`crates/tx-time`** + 服务门面 `crates/tx-services/src/time/`（`timekeeper` / `ClockId` / `DeadlineRegistrar` / `VvarPublisher` 等）；`crates/tx-subsystems/src/wall_clock.rs` 在 main 上**已删除**。net 的消费点只有 `init/net.rs:230` 把 `P::read_ns()` 换成 `timekeeper_clock::<P>().monotonic_now_ns()`。
- **reactor baseline**：`tx-reactor/src/timer.rs` 整文件删除，换成 `deadline_registry.rs`(+328)；`current_timer_wheel` → `current_deadline_registrar(hart)`；`wait.rs` +353 引入 **post 机制**（`fire_with_post` 族）—— 这是 net 侧所有 `_with_post` 重命名的根源。

**非 net 的价值盘点**（合并 main 真正买到的东西）：

| 区域 | 内容 | 规模 |
|---|---|---|
| 观测 L0–L6 | `xtask/src/observe_schema.rs` +2551、`schema/txobserve.toml` +1454、L2 producer、L6 views | ≈9100 |
| ELF exec / loader | `init/exec.rs` 重写 3359、`exec/script.rs` +1698、`exec/loader.rs` +957 + 测试 +3338 | ≈9800 |
| page_backed / mount / device | `page_backed/mod.rs` +2442、`mount/mod.rs` +1491、`vfs/fd_ready.rs` +436 | ≈8600 |
| 架构 lint | `lint_invariants_time_layering.rs` +1373、`time_wake` +1172、`step_interface`、`observe`、`zone` 等 10 个新文件 | +7602 |
| syscall 层与测试 | `io.rs`、`fs_basic.rs`、`fd_ops_wave2` +1155、`fcntl_misc` +808 | ≈5300 |
| io_manager | `page/service.rs` +2683、`block/mod.rs` +863、`backend/plan.rs` +664 | ≈4600 |
| ext4 日志 | `journal.rs` +2019、`planner.rs` +1062（JBD2 风格事务日志） | ≈4150 |
| 时间子系统 | 新 crate `tx-time` + `tx-services/src/time/` | ≈3000+ |

外加 la64 HAL 近乎重写、rv64 pmap/trap/sbi/dtb、VF2 上板 —— 这些是必须拿的。

> 注：main 新增的 `crates/tx-shims/src/linux_syscall/net.rs`(+401) 文件头自述 "Minimal local socket shim for OSComp libc smoke tests. This is intentionally not a network stack." 是给 libctest 的假 socket，不是网络能力。

---

## 四、依赖面 —— "网络树整体取 feature" 的编译代价

前提：强取 feature 的 `crates/tx-subsystems/src/net/`、`crates/tx-shims/src/linux_syscall/socket/`（目录）、`crates/tx-kernel/src/init/net.rs`、`external/smoltcp-asterinas/`；其余用 main。
注意 **`linux_syscall/socket.rs` 是文件不是目录**，按此计划归 main。

### 方向 A：main 非 net 代码 → feature net 树中不存在的符号（8 处，纯降级式改动）

| 调用方 | 缺失符号 | 修复 |
|---|---|---|
| `tx-drivers/src/virtio/net.rs:11,378` | `net_delegate_kick_poll_with_post` | 改回 `net_delegate_kick_poll()`，丢弃 post 实参 |
| `linux_syscall/socket.rs:15,738` | `netlink_netfilter_send_with_post` | 去 `_with_post` 后缀，删 post 闭包 |
| `linux_syscall/socket.rs:16,724` | `netlink_route_send_with_netns_resolvers_and_post` | 同上 |
| `linux_syscall/socket.rs:17,733` | `netlink_xfrm_send_with_post` | 同上 |
| `linux_syscall/socket.rs:19,22,23,24` | `step_process_loopback_udp_with_post`、`step_send_udp_loopback_kernel_bytes_with_post`、`step_tcp_loopback_handshake_with_post`、`step_tcp_loopback_transfer_with_post` | 删 import / 去后缀 |

已核查 main 的 `socket.rs` 只调 `drive_tcp_loopback_after_connect` / `drive_loopback_pending` / `sendto_can_drive_loopback_inline`，三者 feature 侧均有 → 只需修 import。**半小时级。**

### 方向 B：feature net 树 → main 上已删除的非 net API（**真成本在这里**）

`wait_source` 的注册模型换了：main 上 `register_wait_queue_with_id` / `register_wait_port_with_id` / `wait_on_token` / `register_wait_channel` / `release_wait_channel` **全部消失**，换成 `register_wait_source_with_id(id, Arc<WaitSource>)` / `wait_on_source_id` / `wait_on_registered_source_id` / `wait_on_endpoint`。

| 调用方 | 缺失符号 | 严重度 | 修复 |
|---|---|---|---|
| `net/structure/identity.rs:106-108` | `register_wait_queue_with_id` | 阻断编译 | 需把 `RawQueue` 真正适配成 `Arc<WaitSource>`，**不是改名能过的** |
| `net/structure/identity.rs:109` | `register_wait_port_with_id` | 阻断编译 | 同上，`RawPort` → `WaitSource` |
| `net/delegate/runtime.rs:134,165` | `wait_source::wait_on_token` | 阻断编译 | 改走 `registered_wait_for_yield` / `wait_on_registered_source_id` |
| `net/facade/driver.rs:48-53` | `wait_source::wait_on_token` | 阻断编译 | 同上（main 该处已是 `registered_wait_for_yield(shape)`） |
| `net/tests/checks_bind_tests.rs:403,493` | `wait_on_token` | 阻断编译（test） | 同上 |

### 非编译但必须一并处理的两条

1. **时钟桥没人喂**：feature 的 `linux_syscall/mod.rs:984` 有 `net::clock::net_set_now_ns(P::read_ns())`，main 版 `mod.rs` 无此调用（全树 0 命中）。不补回去 → 合完是"能编译的坏网络"，syscall 路径上 smoltcp 定时器只能靠 delegate 刷新。
2. **lint 会红**：`lint_invariants_notification` 的 ratchet ceiling = 0，feature net 直接调 `wait_on_token` 会让 `cargo xtask lint` 变红 → 需照抄 main 的 `net/notification.rs` 并把调用收进去。
3. **未验证的风险**：main 新增的 `lint_invariants_time_layering.rs`(+1373) 是否判 feature 的 `net/clock.rs` 全局 `AtomicU64` 时钟桥违规 —— 需实跑 `cargo xtask lint` 确认。

### 白拿的部分

`external/smoltcp-asterinas/`：`git diff b83a73d3..main -- external/smoltcp-asterinas/` **无输出**，main 完全没动。强取 feature 版（含接收端 SWS 避免）是零冲突纯收益。

### 总计

**12 个阻断编译的断裂点**（方向 A 8 个 + 方向 B 4 个生产代码点），另有 1 个 test-profile 断裂、1 个 CI lint 红、1 个运行期时钟回归、1 个未验证的 lint 风险。集中在 3 个文件约 15 处引用。

**可行性判定：机械上可行，但不是路径级 `checkout` 就完事。** 预估 **0.5–1 人日**手工移植（主要是 `identity.rs` 的四个 wait carrier 适配新 `WaitSource` 抽象）+ 一轮 net 回归。

---

## 五、已定性 / 仍需注意

1. **PR #54 的回退是事故，不是有意的。**（2026-07-27 用户确认）队友不熟悉这部分代码，合并由 AI 执行，
   两边都不知道回退发生了。**修复责任在本人**，不需要与作者重新谈架构。
2. ~~R4a / epoll-on-socket 的失效是静态推断~~ → **已实证为 PR#54 引入的回归**，见第 2.4 节。
3. **MERGE_PROMPT.md 第四节的回归基线是 main** —— 但现在的 main 已经包含 PR#54 的网络回退。这意味着 net 相关的 LTP/单测基线会显著偏低，"相对 main 无回归"对网络部分不再是有意义的判据。非网络部分仍然必须以 main 为基线（那是上次翻车的地方）。

---

## 六、可选策略

| 策略 | 做法 | 代价 | 风险 |
|---|---|---|---|
| **A. 网络树整体取 feature** | merge main，net 全路径强取 feature，再补 12 个断裂点 + 时钟桥调用 + notification.rs | 0.5–1 人日 + 一轮回归 | 与 main 的网络栈正式分叉；PR#54 的 reactor post 机制在 net 内退化 |
| **B. 先与 PR#54 作者对齐** | 暂停合并，确认回退意图 | 沟通时间 | 若对方认为回退是有意的，需重新谈架构 |
| **C. 在 main 的新基线上重做 net** | 接受 main 的 reactor/时间基线，把 P0–P4 + IPv6 的能力重新移植上去 | 数人日起 | 成本最高，但产出唯一一份不分叉的网络栈 |
| **D. 按 MERGE_PROMPT 字面执行** | 只裁决冲突文件 | 最低 | **不可接受** —— 静默丢失外部 connect / DNS / IPv6 / git clone 能力 |
