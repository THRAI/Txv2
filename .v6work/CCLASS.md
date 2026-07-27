# C 类清单 —— main 在 net 里的真新增（合并时必须回植）

2026-07-27 · 配套 `.v6work/PR54_AUDIT.md`（回退面）与 `.v6work/MERGE_PLAN.md`（执行计划）

---

## 〇、结论：79 个 net 文件里，只有 2 项需要回植

「网络树整体取 feature」这个策略**基本安全**，但不是完全安全。全量筛完，需要回植的只有 2 项，
另有 4 项虚警（回植反而有害）和 1 项依赖阻断（搬不动也不该搬）。

| 编号 | 能力 | 判定 | 回植成本 | 价值 |
|---|---|---|---|---|
| **C-1** | 设备改名 `ip link set dev X name Y` + 跨 netns 搬移后改名 | **真缺口** | 中 | 新能力 |
| **C-2** | SOCK_DGRAM ICMP（ping socket）recv 不带 IPv4 头 | **真缺口** | 低 | **修好 feature 上 5 个当前红的测试** |
| 虚警 1 | raw ICMPv4 外部目的地路由分流 | 虚警，**回植有害** | — | — |
| 虚警 2 | raw ICMPv6 未知 peer 判 EOPNOTSUPP | 虚警，feature 更强 | — | — |
| 虚警 3 | host 网桥学习容器邻居 | 能力已有，仅缺测试 | 低（可选搬测试） | 覆盖 |
| 虚警 4 | 容器不把 off-link 网关学成直连邻居 | 能力已有，仅缺断言 | 低（可选） | 覆盖 |
| 阻断 | `delegate/timer.rs` 接 `DeadlineRegistrar` | 纯适配，非新能力 | 高，且**不该搬** | 负（丢可中断语义） |

---

## 一、筛选方法（为什么可以只看这么少）

分三步机械缩圈，把 715 个 main 变更文件收敛到 34 个需要人看的：

**Step A — 只有 main 改、且在 feature 领地外的文件：不看。**
715 → 116（两边都改 35 + net 面 79 + feature 改 main 删 2）。

**Step B — blob 身份判纯还原。**
建历史 blob 表（`git rev-list b83a73d3 -- net` 的 135 个 commit × 全部 net 文件 = 794 个唯一
(blob, path) 对），拿 main 的 102 个 net 文件反查：

> **50 个文件的 blob 与某个历史祖先逐字节相同** —— 纯还原，零新增编辑，**不用看**。

剩 51 个「旧快照 + 新编辑」+ 1 个全新文件（`notification.rs`）。

**Step C — 提 delta。**
对这 51 个，逐个在历史版本里找出**diff 最小的那个祖先 blob**（= dd9435f3 回退到的基线），
`git diff <基线blob> main:<路径>` 得到的差**就是 main 在旧快照之上新加的工作**。
再按含不含 `post`/`mailbox_ref` 预筛掉 reactor post 机制的机械改名，
剩下非-post 变更 ≥10 行的 **34 个文件**才进人工分类。

三批 agent 逐文件分类 → C 类候选 → 每项用 `git grep` 核实 feature 是否真的没有（防"换名字实现"
造成的虚警）→ 关键项由我实跑测试 + main 基线对照实验坐实。

---

## 二、C-1：设备改名

### main 有什么

| 位置 | 内容 |
|---|---|
| `namespace.rs:210` / `:220` | `NetNamespaceDeviceLink` 新增 `name_override: Option<&'static str>` 字段 + `fn name()` |
| `namespace.rs:494` | `set_device_name_by_ifindex` —— 含 lo 拒绝、`EEXIST` 冲突检测、旧名路由 `oif_name` 迁移、连接路由抑制表清理、快照缓存失效 |
| `namespace.rs:632` | `has_device_name_conflict` |
| `namespace.rs:1671` | `name_for_device` |
| `rtnetlink.rs:920-923` | `apply_setlink_info` 开头读 `IFLA_IFNAME` + `validate_ifname` |
| `rtnetlink.rs:957, 985-997` | 记录 `moved_to`，搬移完成后在**目标 netns** 按旧名定位再改名 |

配套改写 3 处消费点：`link_snapshot` 改用 `link.name()`（顺带把 5 次逐字段加锁合并成一次
`namespace_devices.lock().clone()`）；`find_device_by_name` 改为 **namespace 链接优先、host 注册表
兜底且受 `host_devices_visible` 约束**；iface runtime 构造改用 `link.name`。

### feature 确认没有

```
git grep -n "set_device_name_by_ifindex|name_override|has_device_name_conflict|name_for_device" HEAD -- crates/
→ 全部无输出
```

语义侧核实（防换名实现）：feature 的 `IFLA_IFNAME` 只出现在 `handle_newlink`、dump 出方向、
veth peer 解析三处，**`apply_setlink_info` 函数体内一处都没有**——它只处理 `IFF_UP` / `IFLA_MTU` /
`IFLA_MASTER` / `IFLA_NET_NS_FD` / `IFLA_NET_NS_PID`。

feature 的 `NetNamespaceDeviceLink` 与 main 逐字段相同，**只差 `name_override`**；
`find_device_by_name` / `link_snapshot` / `device_snapshot` / `is_device_up` 与基线 blob 逐字节一致
→ feature 在这块完全没动过。**真缺口，非虚警。**

### 回植

- **成本：中**。namespace.rs（结构体 +1 字段、**5 处结构体字面量补 `name_override: None`**、
  3 个新方法、3 处消费点改写）+ rtnetlink.rs（+23 行）+ 2 个测试文件。
- **依赖：无 A 类、无 B 类。** 所需基础设施 feature 全有：`validate_ifname`、`leak_ifname`、
  `attr_string`、`forget_connected_route_suppressions_for_oif`、`invalidate_link_snapshot_cache`、
  `net_device_snapshot`。
- ⚠️ **副作用面**：`find_device_by_name` 改成 namespace 优先，会同时影响 veth 的 `EEXIST` 检查
  和 `default_veth_peer_name`，**必须一并回归**。
- ⚠️ **行为细节**：指定了 `IFLA_NET_NS_*` 但在目标 ns 按旧名找不到 link 时，改名被**静默跳过**
  （无 else 分支）；同名改名走 `else if name != target.name` 直接 no-op，不报 `EEXIST`。
  回植时决定是否保留这个宽容语义。

### 配套测试（可直接搬，helper feature 全齐）

- `rtnetlink_setlink_netns_pid_can_rename_moved_veth_peer`（72 行）
- `namespace_runtime_alpine_docker_nat_peer_rename_renders_conntrack_procfs`（209 行）
- `rtnetlink_newaddr_initial_namespace_uses_host_visible_ifindex_not_eth0`（50 行）
  —— 这是 `find_device_by_name` 改写的回归护栏

---

## 三、C-2：SOCK_DGRAM ICMP 不带 IPv4 头 ⭐ 价值最高

### main 有什么

`protocol/icmp.rs:221` / `:266`：`recv_len` / `recv_bytes` 各裂出一个带 `include_ipv4_header: bool`
的变体。`true` → `icmpv4_echo_raw_packet_len` / `build_icmpv4_echo_reply()`（带 20 字节 IP 头）；
`false` → `icmpv4_echo_message_len` / `build_icmpv4_echo_reply_message`（只回 ICMP 报文）。

开关在消费端 —— `structure/payload.rs:743` 和 `:1184` 传 `self.is_raw_icmp_socket()`：
**`SOCK_RAW` 带头、`SOCK_DGRAM` 不带头**，正是 Linux ping socket 的语义。

### feature 确认没有

```
git grep -n "recv_bytes_with_ipv4_header|recv_len_with_ipv4_header|include_ipv4_header" HEAD -- .../net
→ 无输出
```
feature 的 `recv_len`(:249) / `recv_bytes`(:280) 函数体内无条件用 `icmpv4_echo_raw_packet_len` 和
`build_icmpv4_echo_reply(packet).as_bytes()`，**恒带 IP 头**；
`is_raw_icmp_socket` 在 feature 存在（`payload.rs:1231`）但**没用在 recv 路径**上。

### ⚠️ 实证：feature 上有 5 个测试因此现在就是红的

不是静态推断，是跑出来 + main 基线对照：

```
net::tests::bridge_tests::namespace_runtime_drives_container_ping_container_through_bridge
  feature: FAILED —— left: Some(40)  right: Some(20)      ← 40 = 20(IP头) + 20(ICMP报文)
  main   : ok
```

逐个隔离复跑（`bridge_tests` 全组跑会因 epoch 锁中毒级联，必须隔离才能判真伪）：

| 测试 | feature | main |
|---|---|---|
| `namespace_runtime_drives_container_ping_container_through_bridge` | **FAIL** | PASS |
| `namespace_runtime_drives_container_ping_host_gateway` | **FAIL** | PASS |
| `namespace_runtime_masquerades_icmp_and_conntrack_dnat_reply` | **FAIL** | PASS |
| `namespace_runtime_relearns_container_gateway_after_arp_delete` | **FAIL** | PASS |
| `namespace_runtime_retries_masqueraded_forward_after_uplink_arp_resolution` | **FAIL** | PASS |
| （另 5 个是级联受害者，隔离后 PASS） | PASS | — |

**所以 C-2 不只是补能力，它能把 feature 现有的 5 个红测试转绿。**

### 回植

- **成本：低**。icmp.rs 加两个变体（约 30 行）+ payload.rs 改 2 处调用点 + bridge_tests 6 处断言
  改成 `20 + message.len()`。
- **依赖：无。** helper feature 全有：`build_icmpv4_echo_reply_message`(icmp.rs:519)、
  `icmpv4_echo_message_len`(icmp.rs:523)、`is_raw_icmp_socket`(payload.rs:1231)。
- ⚠️ **回植时避开一个坑**：main 的实现把 `build_icmpv4_echo_reply(packet).as_bytes()` 改成了
  `.as_bytes().to_vec()`，**给每次 raw recv 都加了一次堆分配**。应保留 feature 的零拷贝分支，
  只在 `include_ipv4_header == false` 时才 `to_vec()`。
- **要补测试**：main 没给这个能力配 dgram 测试。feature 现有的
  `raw_icmp_ipv4_recv_returns_ip_header_for_raw_socket` 只覆盖 RAW 带头路径（回植后仍绿），
  需要新写一个 `ValidSocketType::validate(2, 2, 1)` 的 dgram 不带头测试。

---

## 四、虚警（核实过，不要回植）

### 虚警 1：raw ICMPv4 外部目的地路由分流 —— **回植会造成回归**

main（`step_send.rs:255-258, 903-916`）用 `raw_icmpv4_route_uses_gateway` /
`raw_icmpv4_route_uses_device` 判「有没有路由」来决定合成本地回包 vs 走真网线。

feature（`step_send.rs:256-267`）用白名单
`ipv4_multicast_group_is_joined_locally(dst) || ipv4_addr_is_configured(dst)`，
来自 `14011f3b「外部 raw ICMP 接设备 TX — QEMU 真机外部 ping6 3/3 全通」`。

`send_configured_icmpv4_echo` 函数体两边**逐行相同**，内含
`if !multicast_local && !ipv4_addr_is_configured(dst) { return Err(EOPNOTSUPP) }`。于是：
**dst 完全没有路由时** → main 的门全通过 → 进函数 → 立刻 `EOPNOTSUPP`；
feature 白名单不命中 → 落到通用 reserve → device-TX → **上线**。

> main 的黑名单门是这条能力的**早期不完整版本**。回植它会把 `14011f3b` 修好的外部 ping 打回
> `EOPNOTSUPP`。**不回植。**

### 虚警 2：raw ICMPv6 未知 peer

main：`raw_icmpv6_unknown_peer_addr_stays_unsupported` 断言 `Err(EOPNOTSUPP)`。
feature：`raw_icmpv6_unknown_peer_addr_queues_external_echo` 断言 `Done(len)` 且
`peek_icmp6_tx_echo() == Some(request)`。**feature 严格更强**（把"不支持"升级成"排进 v6 device-TX 队列"）。
回植 main 版本是功能倒退。

### 虚警 3 / 4：两个测试覆盖增强（可选搬）

- `namespace_runtime_container_ping_host_gateway_learns_bridge_neighbor`（103 行）——
  逐行对比确认两边 ARP RX 处理与 `learn_arp` 本体**逐字节一致**，投影函数 feature 已有
  （`project.rs:105`）。main 只是补了"host 网桥侧"这一方向的覆盖。**值得搬，但依赖 C-2 的
  `20 +` 断言语义，要一起搬。**
- 容器不得把 off-link 网关学成直连邻居 —— 在既有测试尾部追加 ~12 行断言。能力两边一致。

---

## 五、依赖阻断：`delegate/timer.rs`（83 行，本批最大，但不该搬）

main 把 `net_delegate_wait_tick_deadline` 从「`Channel` + `WaitProtocol::InterruptibleTimeout`」
改成泛型 `<R: DeadlineRegistrar>` + 新写的 `NetDelegateDeadlineFuture`。

**判定：(a) 纯适配新 reactor API，不是新增定时能力。** 三条证据：

1. 语义未扩展——旧路径「到点发 TICK」，新路径也是「到点发 TICK」，
   `smoltcp_instant_to_reactor_deadline_ns` 一字未动。
2. 配套测试只换句柄类型，断言逐条相同（`reactor.channel()` → `reactor.deadline_registrar_handle()`）。
3. **实际是能力收窄**：feature 的 `WaitOutcome` 有 `Ready/Interrupted/Killed/TimedOut` 四态，
   旧 `InterruptibleTimeout` 可返回任意一个；main 新 future 的 `poll` 只有
   `Poll::Ready(WaitOutcome::TimedOut)` 一条出口，**丢掉了可中断/可 kill 语义**。

**依赖**：`tx-time` crate + `tx-services::time/` 9 个文件 + `tx-reactor::deadline_registry` +
`current_deadline_registrar(hart)` —— feature 全无。要搬就得把 PR#54 的 reactor baseline 整块拿过来。

**减压事实**：两个分支里这条路径**都只被测试调用，没有接进生产 driver**。取 feature 版
= 保留旧 `TimerWheel` 路径，**不丢运行时功能**。

> 结论：不搬。接 PR#54 的 reactor baseline 是独立的跨 crate 基础设施迁移，不属于「net 真新增」范畴。

---

## 六、其余全是 B 类（reactor post 机制）

51 个含新编辑文件里，除上述之外的 delta 几乎 100% 是 post 机制的机械改名：
`*_with_post` / `*_and_post` 签名、`post` 闭包参数、`mailbox_ref`、
`fire_recv/fire_send/fire_accept` → `fire_*_with_post`、
`wait_on_token` → `wait_on_registered_source_id`、
`yield_on_token` → `crate::net::notification::yield_on_wait_token`。

按「网络树整体取 feature」策略这些自动消失，只需处理 `MERGE_PLAN.md` 第五节列的编译断裂点。

---

## 七、对 MERGE_PLAN.md 的修订

原计划 Step 2 是「网络树整体取 feature」然后直接进 Step 3。**必须插入 Step 2.5：回植 C 类**。

```
Step 2    网络树整体取 feature（79 个文件一次性夺回）
Step 2.5  回植 C-1（设备改名）+ C-2（DGRAM ICMP 不带头）   ← 新增
Step 3    逐个解非网络冲突
```

建议顺序：**先做 C-2**（成本低、能立刻把 5 个红测试转绿，等于给后续步骤一个更干净的基线），
**再做 C-1**（成本中、副作用面大，需要单独回归 veth EEXIST 与 `default_veth_peer_name`）。

两项都做完再进 Step 3，这样 Step 6 门 3 的单测集合差才有意义。
