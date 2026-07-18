# P3-C 执行计划：R 族资源修复 —— backlog 排空 + 所属 ns 表 + 引用环 + conntrack 界老化 + TCP 内存记账

<!-- txdoc:07-NET-P3C-V1 -->

> 阶段来源：[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md) §5-P3 之③（R 族修复）。前序 [`REFACTOR_P3_v1.md`](REFACTOR_P3_v1.md)（P3-A 上半接入）、[`REFACTOR_P3B_v1.md`](REFACTOR_P3B_v1.md)（P3-B 模型瘦身）已收官。取证：两路并行调查（生命周期/回收路径、conntrack/内存记账）+ orchestrator 亲验 v2 D12 与审计 R2 族。所有 file:line 按 `4d256a4b`。

---

## 1. 病根清单（审计 R2b/c/d/e/f + R1c/e 残余，方案 D12）

**R2b 监听关闭不排空 backlog**：`step_socket_close` 的 Listening 分支只 `withdraw_tcp_listener + withdraw_tcp_bound`（step_socket_close.rs:48-51），全程不碰 `tcp_backlog`（payload.rs:124）。**关键连锁（取证）**：**connected（待 accept）child 在握手完成时双注册**——`insert_tcp_connection`（step_connect.rs:220）+ `enqueue_accept_entry`（step_connect.rs:231），所以 listener close 不排空时它的强 `Cap` 仍钉在连接表里没人递减 → 泄漏（320KB+identity+ns 引用+连接槽）。connecting（半开）child 只在 backlog Vec 里，随 payload drop 释放。

**R2c cleanup 用错 ns 表**：`step_tcp_cleanup` 的 `withdraw_connection_key` 硬编码全局 `SOCKET_TABLE`（=初始 ns，step_tcp_cleanup.rs:5/68），非 `socket.net_namespace().socket_table()`。**范围收窄（取证纠正）**：fd-close 主路径 `step_socket_close` 用的是正确表（step_socket_close.rs:43/61）；R2c 只在**次路径**——graceful close `step_tcp_close_staging → cleanup_tcp_connection`（step_tcp_close.rs:42）与 `step_tcp_connection_cleanup`（step_tcp_cleanup.rs:24）——命中错表，非初始 netns 连接经此永不删除。

**R2f 引用环**：`SocketPayload.net_namespace` 是强 `PayloadCap`（payload.rs:118），`SocketTable` 全 13 个 Index 存**强** `Cap<SocketIdentity>`（table.rs:117-129，无 Weak），环 = ns→table→Cap→payload→net_namespace→ns。close 不排表则 ns 被钉死（ns Drop 注释自证，namespace.rs:307-327）。**R2f 是 R2b/c 的后果**——排表修好（B/C）则环自解，不需独立机制。

**R2d TCP 320KB 无记账**：`RawTcpSocket::new` 无条件裸 `vec!` 预分配 recv 256KB + send 64KB = 恰 320KB（tcp.rs:469-470，尺寸 types.rs:724-725），loopback/外部无差异、无延迟、无全局上限/记账。**现成模型（取证）**：UDP 的 `UDP_SMOLTCP_BACKING_MAX_BYTES=32KB` clamp（udp.rs:24/560-562）把实际 backing 与上报 capacity 解耦——直接可复制到 TCP。

**R2e conntrack 无界无老化 O(n)**：masquerade + dnat 两张裸 `Vec`（netfilter.rs:138-139），entry 无 TTL/last_seen（netfilter.rs:148-168），无上限、无 evict、reply 查找 O(n) 线性扫（netfilter.rs:774/891）；每转发包 prerouting+postrouting 各插入+扫描（namespace.rs:1882/1955）→ 多流无界增长 + 每包二次退化。**现成范式（取证，同模块）**：ARP cache `expires_at` 惰性过期（ether.rs:23/38/626-636）+ IPv4 重组表容量上限（ether.rs:195/794）。

**R1c 部分残留**：P3-B 把 `protocol_state` 并入 `TcpInner`（同 smoltcp 锁），但 `SocketProtocol` 意图 FSM 仍是 `SocketPayload` 上第二把锁（payload.rs:119）；`tcp_connected_peer_error` 先 `protocol_snapshot()`（payload 锁，step_send.rs:338）再 `raw.may_send()`（TcpInner 锁，step_send.rs:348），两次独立 acquire 无原子。**裁量**：FSM 分离是刻意设计（payload.rs:36-38 注释），双读窗口的实际后果是 EPIPE 判定偶发错窗——影响面远小于 R2；本波**只记账不改**，避免动 194 触点的 FSM。

**R1e bind check-then-act**：`*_bind_conflict` 查后 `table.bind_*` 写非原子（step_bind.rs:159-168/297-318），reuseaddr 替换是 withdraw+bind 两步。**裁量**：需要 table 层"查+插"复合原子入口（`bind_if_absent`），是 SocketTable 的独立小改，可做但与 R2 正交——列为本波**可选 S6**，撞上再做。

**R2a（时钟冻结致老化失效）已由 P0 结构性拆掉**（NET_NOW_NS 桥+常驻 iface），conntrack 加老化后能真跑——本波不再涉及。

---

## 2. 分步实施（S1–S6，每步独立编译/验证/提交/可回滚）

### S1 —— R2b：listener close 排空 backlog（判决性泄漏修复）

- `step_socket_close` 的 Listening 分支：close 前遍历 `tcp_backlog` 两队列——`connected`（SocketAcceptEntry）每个 child 先 `withdraw_tcp_connection(server_key)` 从连接表撤销（递减强 Cap）再随队列清空 drop；`connecting`（TcpBacklogEntry）随清空 drop。复用既有访问器（accept_queue_len/pop_accept_entry/connecting_children，payload.rs:582/1071/1056）+ 连接表撤销（step_socket_close.rs:60-62 同款）。
- **判决单测**：listener bind→listen→注入两个 SYN 完成握手（两个 connected child 入连接表+accept 队列）→ close listener → 断言连接表两项已撤销 + backlog 空 +（若可）child payload 不再 live。这是 R2b 的灵魂测试（取证确认此类测试**当前缺失**）。

### S2 —— R2c：cleanup 用所属 ns 表

- `step_tcp_cleanup` 的 `withdraw_connection_key`：`SOCKET_TABLE.withdraw_tcp_connection` → 从 socket 的 `net_namespace().socket_table()` 撤销（对齐 step_socket_close.rs:43/61 的正确取法）。graceful close 次路径（step_tcp_close.rs:42）随之修正。
- **判决单测**：非初始 netns 建连接→走 graceful close/cleanup 路径→断言该 ns 表项已删（非初始 ns 表，不是 SOCKET_TABLE）。取证确认此类测试缺失（现有非初始 ns 测试都走正确的 step_socket_close 主路径，掩盖了 R2c）。

### S3 —— R2f 验证：引用环随 B/C 自解

- 不新增机制。加一个判决单测证明 S1+S2 之后环已解：建 socket 于隔离 ns→close→drop 掉所有外部 Cap→断言 ns payload 可回收（retain_count 归零 / is_payload_live 转 false）。取证确认"引用环不泄漏"断言当前为 0。
- 若测试暴露仍有残留强引用（如某表未撤销），回补到 S1/S2 的排空清单。

### S4 —— R2d：TCP buffer 上限 + 差异化（照抄 UDP 范式）

- 引入 `TCP_SMOLTCP_BACKING_MAX_BYTES`（对齐 UDP 的 32KB，或按 TCP 吞吐需要设更大常量如 64KB），`new_smoltcp_tcp_socket` 的 `vec!` 分配经 clamp——实际 backing 与上报 `recv_capacity/send_capacity`（SO_RCVBUF/SNDBUF 语义）解耦，同 UDP（udp.rs:93-103）。每 socket 实占从 320KB 降到 ≤2×cap。
- **不做**全局 socket 内存记账/substrate reservation（那是更大工程，且无 EMFILE/上限基建）——记账入 P4/后续，本波只砍单 socket 的过度预分配。
- **验证**：单测断言 clamp 后实 backing ≤ 上限而上报 capacity 不变；bulk 32KB 冒烟不退化（发送吞吐不受 clamp 影响验证）；loopback/外部冒烟全绿。

### S5 —— R2e：conntrack 加界 + 老化 + 非线性（照抄 ARP/重组范式）

- 两张 `Vec` → `BTreeMap<key, entry>`（key=5-tuple，reply 查找从 O(n) 变 O(log n)）；entry 加 `expires_at: Instant`（ARP 范式）；insert 刷新 expiry + 容量上限（重组表范式，满则拒新流或 evict 最旧）；查找/tick 惰性过期（`expires_at <= now` 即 remove）。now 来自已解冻的 NET_NOW_NS（P0）。
- **验证**：单测——插满到上限拒/驱逐；过期项在 now 推进后被剔除；reply 查找命中。bridge_tests 的 conntrack 断言（len==1 等，bridge_tests.rs:921+）不退化——现有 30 处断言是回归网。

### S6（可选）—— R1e：bind 查+插复合原子

- 若 S1-S5 顺利且有余量：SocketTable 加 `bind_tcp_if_absent`（查+插一次锁），step_bind 各 `*_bind_conflict`+`bind_*` 收敛。撞上并发 bind 问题才做，否则记账 P4。

---

## 3. 测试方案

1. **判决单测（本波核心，取证确认全缺失）**：S1 listener-close-排空-回收；S2 非初始-ns-cleanup-正确表；S3 close-后-引用环-解开；S4 TCP-backing-clamp；S5 conntrack-界+老化+查找。
2. **回归网**：host 集合差（P3-B 基线 312 + 已知新测试名）；六冒烟（ext/tcp-lo/udp-lo/dns/seq/epoll）+ accept + bulk32K；bridge_tests conntrack 30 断言（S5 直接回归）；busybox-boot + la64。
3. **内存/泄漏的判决方式**：R2b/c/f 以"表项撤销 + payload 可回收"断言交付（结构性），非跑海量 socket 看 OOM（无该基建，且 TCG 慢）；net_stress 海量连接实证挂有镜像/多核环境轮。
4. **暂缺**：LTP netns/bridge 全量（无镜像）；`-smp 4` 并发（多核不稳）——挂环境轮。

---

## 4. 风险与已知坑

1. **S1 排空的锁序**：遍历 backlog（tcp_backlog 锁）内做连接表撤销（table 无锁 Index）——确认不与 close 已持的锁冲突；child 撤销用 server_key（entry 自带 local/peer），别误撤 listener 自身。
2. **S5 conntrack 换容器**：masquerade/dnat 的 key 语义（5-tuple 方向）要与现有线性去重逻辑严格等价，bridge_tests 30 断言是防回归网；NAT reply 匹配的方向别搞反。
3. **S4 clamp 语义**：SO_RCVBUF/SNDBUF getsockopt 必须仍上报 capacity（不是 clamp 后的 backing），否则 LTP sockopt 测试退化——UDP 已是此语义，照抄。
4. **R2f 顺带纠正**：审计记的 SocketTable Box::leak Drop 的 SAFETY 注释推理有误（称 Index 无 Drop，实则 index.rs 有）——若 S3 触及该文件，顺手订正注释（不改行为）。
5. **回滚单元 = S 步**；S1/S2/S4/S5 相互独立可分别 revert。

---

## 5. 设计点拍板（按既定授权取推荐）

| # | 问题 | 取向 |
| - | --- | --- |
| 1 | R2f 修法 | **不新增机制，靠 S1/S2 排表自解 + S3 断言验证**；独立 Weak 化连接表是大改，无必要 |
| 2 | R2d 范围 | **只砍单 socket 过度预分配（clamp，照抄 UDP）**；全局内存记账/reservation 挂 P4（无 EMFILE 基建） |
| 3 | R2e 容器 | **BTreeMap + expires_at 惰性老化 + 容量上限**（ARP+重组范式）；LRU 精确驱逐非必需，容量满拒新流即可 |
| 4 | R1c/R1e | **R1c 只记账不改**（FSM 分离刻意、194 触点、后果小）；**R1e 列可选 S6**（撞上再做） |

---

## 6. 实施记录（2026-07-03 完成，S1–S5 + S6 挂账）

<!-- txdoc:07-NET-P3C-V1-DONE -->

- **S1**（c7db9a7f）R2b：`step_socket_close` 三种 listener 分支（TCP/SCTP/UnixStream 共用 tcp_backlog）close 前 `drain_backlog_for_close` 排空两队列，connected child 从对应连接表撤销（TCP `withdraw_tcp_connection`/SCTP `withdraw_sctp_connection`/UnixStream `withdraw_unix_stream_peer(child.raw())`）；`TcpBacklog::clear_connecting` 新增。判决单测=完整握手→connected child 双注册→close→断言连接表撤销。
- **S2**（a39e5669）R2c：`step_tcp_cleanup` 的 `withdraw_connection_key` 从硬编码 `SOCKET_TABLE` 改经 `payload.socket_table()` 用所属 ns 表。判决单测=隔离 ns Connected 连接→cleanup→断言该 ns 表删、初始 SOCKET_TABLE 未动。
- **S3**（a39e5669）R2f：无代码改动，验证测试证明 close 后 `is_payload_live()==false`（断 socket→ns 强链）+ child retain_count 下降（断 table→Cap），引用环随 S1/S2 自解。
- **S4**（167b9e2f）R2d：`TCP_SMOLTCP_BACKING_MAX_BYTES=64KB` clamp（仿 UDP），vec 分配经 `tcp_backing_bytes`；每 socket 320KB→≤128KB。上报值不变（getsockopt 读 options）。**吞吐严格对照**：bulk 32KB 尾段 flaky 经 stash pre-S4 证实为既有外部 close-flush + nc 时序,非 clamp。
- **S5**（b98821e6）R2e：两 conntrack entry 加 `last_seen`；`CONNTRACK_TTL=120s`+`CONNTRACK_MAX_ENTRIES=4096`；insert `expire_and_cap`（retain 过期+满驱逐 LRS）、dedup/reply 命中刷新。bounded 表→bounded 扫描。判决单测（受控 Instant）=过期剔除+cap 驱逐。**裁量**：不改 BTreeMap（reply 非对称查找需反向索引=NAT 方向 bug 藏身处），cap 已把 O(n) 有界化。
- **S6（挂账，未做）** R1e bind check-then-act：需 SocketTable `bind_if_absent` 复合原子入口，且只能 `-smp 4` 决定性验证（本机多核不稳）——与 R1c（FSM 分离记账不改）、D14b、多核并发同族，**移交环境轮**，不做无法验证的改动。

**验收**：每步 host 集合差零真回归（唯一入列的 R2c 测试单跑绿=毒锁级联，同 P3-A/B 惯例）+ 六冒烟 + bridge conntrack netfilter_* 单跑绿 + bulk32K + la64/boot。R2b/c/f 以"表撤销+payload 释放"结构性断言交付；conntrack 无界压力实证挂 net_stress 环境轮。

---

*P3-C 完成：socket/conntrack 资源在 close 与转发路径上有界、可回收；P3 三波（A 接入 / B 瘦身 / C 资源）全部收官，审计 ①–⑩ + R1-R4 主体落地。余账（均已记录）：R1e bind 原子（S6，环境轮）、R1c FSM 双读（记账不改）、R3a 校验和（P4 输入验证）、D14b 等待收敛（LTP 轮）、多核并发实证+net_stress 内存压力（环境轮）、D4 每 iface 锁端态（P5）。*
