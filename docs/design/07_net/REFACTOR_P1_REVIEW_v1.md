# P1 代码审查指南：六个提交逐一过堂

<!-- txdoc:07-NET-P1-REVIEW-V1 -->

**Status.** v1 (2026-07-02)。审查对象 = P1 实施的六个提交 `2fc0a3ea…402a441d`（+文档提交 `0f8b5d38`），基于 P0（`5ab58517`）。设计依据见 [`REFACTOR_P1_v1.md`](REFACTOR_P1_v1.md)（§1 现状地图、§8 验证记录）。

**本文怎么用.** 每个提交一节：**意图 → 关键 diff 讲解 → 审查要点（打勾清单）→ 验证证据**。§7 是需要你签字放行的已知债务。审查时对照 `git show <commit>` 逐节看。

**复验命令（单核）**：

```bash
# host 全量（失败集合对照法——绝对数没有意义，本分支基线本就 305 败）
cargo test -q -p tx-subsystems --lib -- --test-threads=1

# 灵魂测试单跑
cargo test -q -p tx-subsystems --lib tcp_loopback_lost_data_segment -- --test-threads=1

# QEMU 冒烟（单核）
cargo xtask full-build --skip-doctor
qemu-system-riscv64 -machine virt -m 256M -smp 1 -accel tcg -display none -monitor none \
  -serial file:/tmp/s.log -no-reboot \
  -kernel target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt \
  -bios external/opensbi-silent/fw_dynamic.bin -no-shutdown \
  -initrd target/images/busybox-initramfs-rv64-qemu.cpio \
  -append 'tx.profile=busybox init=/bin/tcp-loopback-smoke console=ttyS0'   # udp 同理
grep tx-n68 /tmp/s.log
```

---

## 0. 一图总览：这六个提交合起来干了什么

```
改造前（三条 TCP 通路 + 两条 UDP 直拷 + 影子缓冲）      改造后（smoltcp 单通路）
用户 write → corked/tx_buffer影子+smoltcp ring          用户 write → [MSG_MORE: corked_tx] → send_slice
   ├─ A 直拷流(按 has_connected 分流) ──✂ S0                        │ (smoltcp tx ring = 唯一居所)
   ├─ B 段级+平账(take_tcp_tx_bytes) ──✂ S2                        ▼
   └─ C 外部裸塞+假ACK记账 ──✂ S1                       dispatch → lo 队列 → process
用户 read ← rx_buffer(三个写入方) ──✂ S1                用户 read ← recv_slice (smoltcp rx ring)
UDP: 查表直塞×2 ──✂ S4                                  UDP: egress 编包 → lo 队列 → ingress 投递
SYN-ACK 手工缓存重传 ──✂ S3                             重传/背压/FIN 全归 smoltcp 状态机(P0 活钟)
```

代码净变化：**约 +370/−475 行**（不含文档与测试）——删的是双份记账，加的主要是 UDP v6 臂与竞态加固。

---

## 1. S0 `2fc0a3ea` — 断 TCP 直拷旁路

**意图.** 删掉按 `has_connected` 分流的直拷世界：`tcp_uses_direct_stream` 选择器、`send_tcp_stream_bytes`（查表→`ingest_rx_bytes_unbounded` 无界直塞对端）及两个分发点。

**为什么安全.** loopback 正常连接走真握手（`has_connected=true`）本就在段级路径；落在直拷路的只有外部 demux 手工造的 Connected 连接——外部 TCP 在本分支结构性不可用（审计①），行为从"假成功"变"诚实失败"。

**审查要点.**
- [ ] `grep -rn "tcp_uses_direct_stream\|send_tcp_stream_bytes\|ingest_rx_bytes_unbounded\|record_tcp_stream_bytes"` 全仓为零。
- [ ] `lookup_tcp_connected_peer` 保留（`tcp_connected_peer_error` 还在用）——确认没误删。
- [ ] 语义变化点：手工 Connected 的 socket 现在 send 走 `reserve_send` → smoltcp 未握手 → 拿不到空间 → yield 等待（不再假成功）。接受与否请签字。

**证据.** host 失败集合与基线逐条相同（`comm` 集合差为空）；tcp/udp smoke 冷启动绿。

---

## 2. S1 `12183e11` — 删 `rx_buffer`，recv 直读 smoltcp ring（最大的一步）

**意图.** 用户读从"staging 搬运缓冲"改为直读 smoltcp rx ring；三个写入方各归其位。策略 = **保留 `RawTcpSocket` recv API 签名、只换后备存储**（上层 payload/step_recv 零改动）。

**关键 diff.**

```rust
// tcp.rs — 改动前: rx_buffer(VecDeque) 手工进出;改动后:
pub fn recv_available(&self) -> usize { self.socket.lock().recv_queue() }
pub fn recv_bytes(&self, out, peek) -> Option<(usize, bool)> {
    // peek → peek_slice;非 peek → recv_slice;became_empty = recv_queue()==0
}
pub fn recv_len(&self, len, peek) -> Option<(usize, bool)> {
    // 非 peek = 丢弃式消费:socket.recv(|buf| ...) 循环(ring 可能回绕,单次给不满)
}
```

- 删 `drain_protocol_recv_to_staging`（smoltcp→staging 搬运工）；`poll_context.rs` 的 `recv_has_data` 改为 `recv_readable ||（就绪位未置 && recv_available>0）` 派生。
- **通路 C 收敛**：`TcpPacketEvent` 新增 `segment: Option<SmoltcpTcpSegment>` 字段（`with_segment` 构造，`new()` 缺省 None 保测试兼容）；demux 用 `SmoltcpTcpSegment::parse_ipv4_packet` 产完整段（**校验和验证**，附带修了 R3a 的这条入口）；`process_tcp_event` 已建立连接分支改喂 `process_segment`，**删掉 `ack_bytes = max(1, payload_len)` 的猜测性假 ACK 记账**（events.rs 原 :287）。

**审查要点.**
- [ ] `recv_len` 丢弃循环的终止性：`recv()` 返回 0 即 break；`taken_total` 不会超 `len`。
- [ ] `became_empty` 语义保真：非 peek 且 ring 空 → true → `step_recv` 清 HAS_DATA 位（step_recv.rs:105-107 未改）。
- [ ] **行为差异（需签字）**：MSG_PEEK 在 ring 回绕处只返回第一段连续字节（`peek_slice` 语义），旧 VecDeque 可整段 peek。POSIX 允许短读，但请知悉。
- [ ] EOF 路径不变：`is_recv_shut` flag 仍驱动 `recv_peer_closed`。
- [ ] 无段事件（手工构造）对已建立连接 = 丢弃（`event.segment.as_ref()?`）——只影响外部路径。
- [ ] 校验和验证的副作用：若外部网卡帧无校验和（硬件卸载），parse 失败→丢包——外部 TCP 本就不可用，P2 统一处理；请确认接受。
- [ ] 测试更新（见 §6 对照表）三处是否认可。

**证据.** 失败集合与基线同（仅同槽换名一项，见 §6）；smoke 冷启动绿 = 新 recv 路径端到端工作。

---

## 3. S2 `3553fac1` — 删 `tx_buffer` 影子

**意图.** 发送字节只住 smoltcp tx ring；发送空间、队列深度、发送空间唤醒全部从 ring 派生。

**关键 diff.**

```rust
// send_available: 原 min(影子余量, 协议余量) → 现:
if socket.may_send() { send_capacity - send_queue() - corked } else { 0 }
// send_queued: 影子长度 → socket.send_queue()
// step_process_loopback_tcp: 删 take_tcp_tx_bytes(bytes_moved) 平账 +
// 删手工 peer_space 流控(对端窗口由其 rx ring 天然派生);
// 发送空间唤醒 = 传输前 send_available==0 && 传输后 >0
```

**审查要点.**
- [ ] `send_available` 等价性论证：未连接时 `may_send()=false → 0`，与旧 `min(…, protocol_available=0)` 一致；已连接时旧影子与 ring 同步增长，`min` 退化为 ring 项。
- [ ] `flush_tcp_tx_before_close`（step_socket_close.rs:185）用 `send_queued`——新语义（ring 未发字节）对 close 冲刷循环仍正确。
- [ ] 手工流控删除后的背压：对端 ring 满 → 窗口收缩 → `dispatch` 自然停——`TCP_LOOPBACK_TRANSFER_PACKET_PASSES=64` 上限仍在，无死循环。
- [ ] `corked_tx` 保留（MSG_MORE 未提交暂存，非双份）——拍板 3-A。

---

## 4. S3 `baca7045` — SYN-ACK 重传归 smoltcp 定时器

**意图.** 删手工 SYN-ACK 缓存（`last_syn_ack`），backlog 重传改由 child socket 的 `dispatch_segment` 驱动（P0 解冻后 smoltcp RTO 自会重发）；`has_connected` 闩锁改状态边沿。

**⚠ 本步有一个容易漏看的语义修正（重点审查）**：

```rust
// poll_retransmit_connecting 的闭包契约:返回 false ⇒ 丢弃半连接条目!
// (payload.rs:2150-2152 `if !retransmit(&entry) { outcome.failed += 1; continue; }`)
// 旧闭包几乎总能从缓存发出 → false 罕见;新闭包若沿用旧语义,
// smoltcp RTO 未到期时 dispatch 无段可发 → false → 半连接被误杀。
// 修正:无段可发 = "还没到点",返回 true 保留条目。
let Some(segment) = raw_tcp.dispatch_segment() else { return true; };
```

**审查要点.**
- [ ] 上述 true/false 语义修正是否认可：`attempts` 现在计的是"轮询轮数"而非"实际重发次数"（上限 `TCP_BACKLOG_RETRANSMIT_LIMIT_STAGING` 仍封顶生命周期）；smoltcp 放弃的 child 走 `Closed` 态由 `connecting_entry_failed` 收割。
- [ ] 双层定时器叠加：backlog 1s 回退 × smoltcp RTO 递增——重发时刻 = 两者较晚者，比旧手工节奏慢半拍但正确；请确认接受。
- [ ] `connected` 边沿检测一次性论证：`!before.is_active && after.is_active`；TCP 不经 reset 不会重入活跃集（active = Established/CloseWait/FinWait1/2）。
- [ ] 新增灵魂测试（tcp_lifecycle.rs 尾部）逻辑：丢段→RTO 内 dispatch 必须 None→拨钟→必须 Some→对端收齐。

---

## 5. S4 `123faf5d` — UDP 单通路 + SMP 竞态修复（审查密度最高的一步）

**5a. UDP 直拷之死.** 删 `poll_udp_loopback_direct_one`（poll_context 整函数）与 send 内联的查表直塞段；loopback UDP 一律 `poll_udp_egress_one`（取报→编包→入 lo 队列）→ `poll_udp_ingress`（出队→解析→投递对端队列）。发送路径同步驱动一轮（拍板 4-A，时延特性不变）。

**5b. UDP v6 臂.**（不补就是回归：直拷曾家族无关，iface 转运原仅 v4，v6 回环 UDP 会静默丢包——ipv6_lib/getaddrinfo 类测试依赖它。）`UdpTxDatagram::emit_ipv4_packet` 按 dst 家族分派 `emit_v6`；`UdpRxDatagram::parse_ipv4_packet` 加 v6 落空臂——**镜像 TCP 现成的 v6 处理**（函数名沿革同 TCP：名为 ipv4 实通两族）。

**5c. SMP 竞态修复.** 现象：smp4 下 UDP smoke ~1/3 概率 panic（`cap.rs:349`），S3 基线 6/6 绿——S4 引入的暴露。机理与修复：

```
机理: 队列转运使 poll 路径高频触碰"可能已被并发 close 退休"的外来 socket Cap;
      Cap::deref(cap.rs:349) 与 Cap::clone(cap.rs:280) 对已退休槽 = panic(expect)。
      而 target.acquire_operational() 的自动解引用发生在方法调用之前!

修复: 无 deref 访问模式 ——
  let target_ident = target.downgrade().observe(guard)?;   // 检活 + guard 钉内存
  let target_payload = target_ident.acquire_operational()?; // IdentRef 上调用,无 Cap deref
应用于 poll_udp_egress_one / poll_udp_ingress / NetworkPublishTarget::publish。

第二个坑: publish() 内取 guard 用 epoch::guard() ⇒ 6/6 必炸 ——
      EBR 禁止嵌套 guard(epoch/mod.rs:59-61 自述);
      正确姿势 = borrow_current_guard().unwrap_or_else(guard)。
```

**审查要点.**
- [ ] 5a：`budget=1` 的内联 ingress 可能处理到**别的流**的队头包（自己的包由 delegate 兜底）——"至少一次投递、协作式清队"语义是否接受。
- [ ] 5a：EMSGSIZE/EINVAL/MSG_MORE 早退分支全部保留（对照 diff 确认无误删）。
- [ ] 5b：`emit_v6` 与 TCP 的 v6 parse 镜像逐字段核对（`Ipv6Repr{src,dst,next_header,payload_len,hop_limit:64}`）。
- [ ] 5c：`observe` 之后仍有微小 TOCTOU？——没有 panic 风险：`IdentRef` 内存由 guard 钉住，最坏读到正在关闭的 payload（`live_payload()` 返回 None，各调用点已处理）。
- [ ] 5c：`borrow_current_guard` 的 fallback `guard()`：publish 的所有调用链是否必然已持 guard？（若是，fallback 永不触发；若不是，新开 guard 合法。）两种情况都安全。
- [ ] **残留暴露（签字项）**：TCP 侧 `poll_egress_one`/`poll_ingress`/`process_tcp_event` 等仍是 `Cap` 裸 deref 形态（S3 前就存在，冒烟未见炸），系统性加固归 P3——是否接受这个边界。

**证据.** 修复前 smp4 3 跑 1 炸（S3 基线 6/6 绿可对照）；嵌套 guard 版 6/6 必炸；最终版 smp4 UDP 8/8 + TCP 3/3 绿；UDP 测试族单跑 10/10。

---

## 6. S5 `402a441d` — 时间戳解冻 + 扫尾

- 内联路径 5 处 `PollContext::new_with_table(Instant::ZERO, …)` → `net_now_instant()`（step_tcp_loopback ×2 / step_udp_loopback ×2 / step_icmp_loopback ×1）——backlog `created_at` 等 timestamp 消费者在内联路径不再看到冻结时间。
- **保留的 ZERO**（非本步范围，勿误报）：`step_loopback_pending.rs:255`、`step_device_tx.rs:92`、`step_process_network_events.rs:56` 是无时间参数的便捷包装（测试入口），production delegate 走 `_at` 变体传 `driver.now()`。
- **⚠ fmt 混入（签字项）**：`cargo fmt -p tx-subsystems` 把 `namespace.rs`/`rtnetlink.rs`/`execution/mod.rs`/`step_send.rs` 的既有非 fmt-clean 代码一并重排（注释缩进/导入排序/调用折行），**纯格式无语义**——已逐 hunk 核对。嫌脏可要求我拆出去。

### 测试更新对照表（S 步散布，集中列此——每条都要你认可）

| 原测试 | 处置 | 理由 |
| ------ | ---- | ---- |
| `raw_tcp_socket_ingests_and_drains_rx_bytes` / `raw_tcp_socket_peek_does_not_drain_rx_bytes` | 删除，代之以 `raw_tcp_socket_recv_reports_empty_smoltcp_ring` | 断言的是已删除的 staging 缓冲的有界摄入契约；端到端 recv 覆盖在 loopback_tests |
| `tcp_packet_event_payload_bytes_are_consumed_by_step_recv` | 改写为 `tcp_packet_event_without_segment_is_dropped` | 原断言 = 裸字节旁路投递（病灶本身）；新断言 = 旁路已死。**其前身本就在 305 基线失败集合**（毒化级联族），同槽换名 |
| `tcp_loopback_handshake_connects_bound_client_to_listener` 的 `has_connected` 断言 | 改断 smoltcp `State::Established` | 闩锁已删，"已连接"的单一真相是状态机 |
| 新增 `tcp_loopback_lost_data_segment_is_retransmitted_after_rto` | P1 灵魂测试 | 单跑绿；全量挂于既有毒化级联（同族 17 兄弟基线即挂） |

---

## 7. 需要签字的已知债务（本 P1 明确不修）

1. **LTP 全量对照未跑**——本机无 sdcard 镜像；`recv01/recvfrom01` 冷启动风险以单连接 smoke 作代理。**有 LTP 环境后必须按 P1 §4-3 补验。**
2. **TCP poll 路径的 `Cap` 裸 deref 残留**（同 S4 竞态族）→ P3 系统性加固。
3. **UDP 数据仍在自研队列**（smoltcp `udp::Socket` 惰性）——实施中查实队列同时服务外部 UDP 车道（`step_device_tx` + 外部 RX），P2 外部统一时一并接管（P1 文档 §8"范围修订"）。
4. **MSG_PEEK 短读**（ring 回绕处，见 §2）。
5. **`SO_RCVBUF` 构造时捕获**（setsockopt 后不改 ring 尺寸）——P0 时代已知，未新增恶化。
6. **多核环境**：用户本机多核 QEMU 不稳，验证以单核为准；S4 的竞态修复在 smp4 下 8/8 过，但不构成多核全面背书。

---

*配套阅读：[`REFACTOR_P1_v1.md`](REFACTOR_P1_v1.md)（现状地图 §1 + 验证记录 §8）、[`REFACTOR_P0_WALKTHROUGH_v1.md`](REFACTOR_P0_WALKTHROUGH_v1.md)（P0 逐文件讲解）。行号引用以各 commit 时点为准。*
