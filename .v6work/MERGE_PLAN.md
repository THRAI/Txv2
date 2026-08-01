# main → feature-network-refactor 合并执行计划

2026-07-27 · 基于真实 `git merge-tree` dry-run（不是预测）· 前置审计见 `.v6work/PR54_AUDIT.md`

```
merge-base b83a73d3 ── 12 ──→ 90939012  feature-network-refactor  (你在这)
                    └─ 24 ──→ 8af194db  main  (含 PR#54 net 回退)
```

---

## 〇、三条压倒一切的原则

1. **冲突不是主要风险，静默自动合并才是。** 真实冲突只有 16 个文件；但 main 改了而 feature 这轮没碰的
   net 文件有 **79 个**，git 会**无冲突地取 main 侧**——那是 PR#54 的回退版本。
   MERGE_PROMPT.md 把"网络取 feature"写成冲突裁决规则，**这条规则本身是不够的**。

2. **"非网络取 main"同样是错的。** feature 有 5 个非文档修复 commit，全部落在非 net 的冲突文件上：
   `34737cd4`(sa_restorer/ext4容量/FIB/fatal-trap)、`995ee1aa`(O_TRUNC/mtime)、
   `16a066b1`(dentry 陈旧 walk)、`2c5fe37b`(mprotect PrivatePageSet)、`01aea500`(大包上传)。
   盲取 main 会静默删掉它们。正确规则是 **"非网络以 main 为底 + 回植 feature 的 5 个修复"**。

3. **两个 net 文件已经被 git 自动合并成缝合怪**：`net/execution/step_connect.rs`、
   `net/execution/step_send.rs`（还有 `net/tests/rtnetlink_tests.rs`）。它们**没有报冲突**，
   git 把 feature 的改动和 main 删掉外部 connect 的版本揉在了一起。必须整树强取。

---

## 一、Step 0 — 准备（5 分钟）

```bash
cd /home/msp/learning/Txv2
git status --short            # 必须干净（.v6work/*.md 未跟踪没关系）

# 退路：给当前 feature HEAD 打标签，任何时候可以 git reset --hard 回来
git tag premerge-feature-20260727 90939012
git tag premerge-main-20260727 8af194db
```

基线 worktree 已存在于 `/home/msp/learning/Txv2-main-baseline`（`main` @ 8af194db，已构建 5.4G），
**MERGE_PROMPT.md 第四节第 0 步的 `git worktree add` 跳过**。

---

## 二、Step 1 — 起 merge，不自动提交

```bash
git merge --no-commit --no-ff main
# 预期：CONFLICT ×16，退出码非 0。不要慌，也不要 git merge --abort。
```

此刻工作区状态：
- 14 个内容冲突文件带 `<<<<<<<` 标记
- 2 个 modify/delete 冲突
- **其余全部已经按 git 的判断合并进 index —— 包括那 79 个被静默取自 main 的 net 文件**

---

## 三、Step 2 — Tier 1：网络面整树强取 feature ⚠️ 最关键的一步

**必须在解任何单个冲突之前做。** 这一步按路径覆盖，一次性夺回全部 79 个文件，
不管它们是冲突了、自动合并了、还是被静默取走了。

```bash
git checkout HEAD -- \
  crates/tx-subsystems/src/net \
  crates/tx-shims/src/linux_syscall/socket \
  crates/tx-shims/src/linux_syscall/socket.rs \
  external/smoltcp-asterinas
```

> `HEAD` 在 merge 期间指 feature 侧；`MERGE_HEAD` 才是 main。

**为什么 `socket.rs` 也在里面**：我验过，`socket.rs` **只有 main 改过**（feature 没动），
所以它连冲突都不会报，会被静默取自 main —— 而 main 的版本调用 `netlink_*_with_post` 等
feature 不存在的符号。它是 net 代码，取 feature。

**为什么 `external/smoltcp-asterinas` 也列上**：main 完全没动这个目录，这一步是 no-op，
但显式写出来防止将来有人以为它需要合并。feature 侧的接收端 SWS 修复在这里，是纯收益。

### 2.1 收尾：处理 main 在 net 树里新增的 2 个文件

`git checkout` 不会删除 main 新增的文件，必须单独处理：

```bash
git status --short crates/tx-subsystems/src/net
```

会看到两个：

| 文件 | 处置 | 理由 |
|---|---|---|
| `net/protocol/ether.rs` | **删除**（`git rm --cached` + 截断） | 这是 PR#54 反并回去的未拆分 ether（blob 等于 `a3743b63` 的旧快照）。feature 有 `net/protocol/ether/{mod,link,l3}.rs`，两者共存会直接模块名冲突编不过 |
| `net/notification.rs` | **保留**（见 Step 4.3） | 这是 PR#54 的**真新增**，不是回退。`lint_invariants_notification` 的 ratchet ceiling = 0，没有它 `cargo xtask lint` 会红 |

### 2.2 验证门 A

```bash
# 这三个标记必须都能查到，否则 Tier 1 没生效
git grep -c "try_tcp_external_connect" -- crates/tx-subsystems/src/net/execution/step_connect.rs
git grep -c "CONTEXT_IFACE"            -- crates/tx-subsystems/src/net/protocol/tcp.rs
git grep -c "install_substrate_mirrors" -- crates/tx-subsystems/src/net/structure/identity.rs
ls crates/tx-subsystems/src/net/{clock.rs,file_ops.rs,adapter.rs}   # 三个都必须存在
```

---

## 三-bis、Step 2.5 — 回植 main 在 net 里的真新增 ⚠️ 原计划缺失的一步

Step 2 按路径一刀切取 feature，等于假设「main 在 net 里做的全部是回退」。**这个假设不成立**：
全量筛过 79 个文件后，有 **2 项真新增**必须回植。完整证据见 `.v6work/CCLASS.md`。

> 筛选结论：main 的 102 个 net 文件里 **50 个是纯还原**（blob 逐字节等于历史祖先），
> 51 个「旧快照 + 新编辑」里的 delta 几乎全是 reactor post 机制的机械改名。
> 真需要回植的只有下面 2 项，另有 4 项虚警（回植反而有害）、1 项依赖阻断（不该搬）。

### 先做 C-2：SOCK_DGRAM ICMP recv 不带 IPv4 头（成本低，价值最高）

**为什么先做**：成本低，且能修掉 5 个既有红测试里的一个失败因子。

已实证（隔离复跑 + main 基线对照，不是推断）：

| 测试 | feature | main |
|---|---|---|
| `bridge_tests::namespace_runtime_drives_container_ping_container_through_bridge` | **FAIL** `left: Some(40) right: Some(20)` | PASS |
| `..._drives_container_ping_host_gateway` | **FAIL** | PASS |
| `..._masquerades_icmp_and_conntrack_dnat_reply` | **FAIL** | PASS |
| `..._relearns_container_gateway_after_arp_delete` | **FAIL** | PASS |
| `..._retries_masqueraded_forward_after_uplink_arp_resolution` | **FAIL** | PASS |

⚠️ **实际战果比预期小，2026-07-27 实测更正**：C-2 只修好这 5 个测试的**字节数那一半**。
其中 4 个还有**第二个独立缺陷**——`assert!(total.bridge_forwarded >= 2)` /
`bridge_frames_seen >= 2` 也不过。所以 C-2 落地后**只有
`..._masquerades_icmp_and_conntrack_dnat_reply` 一个转绿**，既有失败基线 −1（不是 −5）。

那 4 个的第二缺陷**与合并无关**，判别实验已钉死：在**纯 feature HEAD**（`premerge-feature-20260727`）
上只打字节断言补丁、不动任何源码，4 个测试失败在**完全相同**的断言上。5 个测试也全都在
`.v6work/unit-baseline.failures`（326 条）里，是记录在案的既有失败。

这是 feature 网络栈相对 main 的一个真实差距（同样的测试同样的断言，main 过 feature 不过；
`bridge.rs` 两分支逐字节相同且不含校验和逻辑，所以差异在到达 bridge 之前）。
**独立立项，不是合并阻塞项。**

改动：
1. `protocol/icmp.rs` —— `recv_len` / `recv_bytes` 各裂一个带 `include_ipv4_header: bool` 的变体
   （`true` 走 `icmpv4_echo_raw_packet_len`/`build_icmpv4_echo_reply`，
   `false` 走 `icmpv4_echo_message_len`/`build_icmpv4_echo_reply_message`），约 30 行
2. `structure/payload.rs:692` 和 `:1149` 两处调用点传 `self.is_raw_icmp_socket()`
3. `tests/bridge_tests.rs` 6 处断言改 `Some(20 + message.len())`
4. 新写一个 dgram（`ValidSocketType::validate(2, 2, 1)`）不带头的测试 —— main 没配

⚠️ **避坑**：main 的实现把 `build_icmpv4_echo_reply(packet).as_bytes()` 改成了 `.as_bytes().to_vec()`，
给每次 raw recv 加了一次堆分配。**保留 feature 的零拷贝分支**，只在 `include_ipv4_header == false`
时才 `to_vec()`。

依赖：无。helper feature 全有（`build_icmpv4_echo_reply_message`、`icmpv4_echo_message_len`、
`is_raw_icmp_socket`）。

### 再做 C-1：设备改名 `ip link set dev X name Y`

feature 的 `apply_setlink_info` **完全不读 `IFLA_IFNAME`**（只处理 `IFF_UP`/`IFLA_MTU`/
`IFLA_MASTER`/`IFLA_NET_NS_FD`/`IFLA_NET_NS_PID`），是真缺口。

改动：
1. `namespace.rs` —— `NetNamespaceDeviceLink` 加 `name_override: Option<&'static str>` 字段 +
   `fn name()`；**5 处结构体字面量补 `name_override: None`**；新增
   `set_device_name_by_ifindex` / `has_device_name_conflict` / `name_for_device`；
   改写 `link_snapshot`（用 `link.name()`）、`find_device_by_name`（namespace 优先、host 兜底且受
   `host_devices_visible` 约束）、iface runtime 构造
2. `rtnetlink.rs::apply_setlink_info` —— 开头读 `IFLA_IFNAME` + `validate_ifname`，记录 `moved_to`，
   末尾在目标 netns 按旧名定位再改名（+23 行）
3. 搬 3 个测试：`rtnetlink_setlink_netns_pid_can_rename_moved_veth_peer`、
   `namespace_runtime_alpine_docker_nat_peer_rename_renders_conntrack_procfs`、
   `rtnetlink_newaddr_initial_namespace_uses_host_visible_ifindex_not_eth0`

⚠️ **副作用面**：`find_device_by_name` 改 namespace 优先会波及 veth 的 `EEXIST` 检查和
`default_veth_peer_name`，**必须单独回归这两处**。

依赖：无 A/B 类。`validate_ifname`/`leak_ifname`/`attr_string`/
`forget_connected_route_suppressions_for_oif`/`invalidate_link_snapshot_cache` feature 全有。

### 明确**不要**回植的（核实过，回植有害）

| 项 | 原因 |
|---|---|
| raw ICMPv4 路由分流（`raw_icmpv4_route_uses_gateway/device`） | main 是早期不完整版。dst 无路由时 main 走 `EOPNOTSUPP`，feature 落到 device-TX 上线。回植会把 `14011f3b` 修好的外部 ping 打回 `EOPNOTSUPP` |
| `raw_icmpv6_unknown_peer_addr_stays_unsupported` | feature 的 `queues_external_echo` 严格更强（"不支持"→"排进 v6 device-TX 队列"） |
| `delegate/timer.rs` 接 `DeadlineRegistrar`（83 行） | 纯适配非新能力，且**丢失可中断/可 kill 语义**（`WaitOutcome` 四态收窄到只有 `TimedOut`）。依赖整个 `tx-time` crate。两个分支里这条路径都只被测试调用、没接生产 driver，取 feature 版不丢运行时功能 |

### 验证门 A-bis

```bash
# 必须逐个隔离跑：全组跑会因 epoch 锁中毒级联，10 个失败里只有 5 个是真的
# 判据(2026-07-27 实测校准)：
#   masquerades_icmp_and_conntrack_dnat_reply     → 必须 PASS(C-2 的战果)
#   其余 4 个                                      → 仍 FAIL 于 bridge_forwarded/bridge_frames_seen
#                                                    属既有缺陷,不是回归
for t in namespace_runtime_masquerades_icmp_and_conntrack_dnat_reply ; do
  cargo test -q -p tx-subsystems --lib -- --test-threads=1 --exact net::tests::bridge_tests::$t
done
# C-2 新增能力的正向覆盖
cargo test -q -p tx-subsystems --lib -- --test-threads=1 --exact \
  net::tests::icmp_tests::raw_icmp_ipv4_dgram_recv_strips_ip_header
```

---

## 四、Step 3 — Tier 2：逐个解非网络冲突

剩下的冲突。**通则：以 main 为底（main 在这些地方更新且更大），然后回植 feature 的修复行。**

我已经用 `git merge-tree` 的合并树核对过 feature 5 个修复的每一行新增，
下表的"feature 必须保住什么"是核对结果，不是猜的。

### 4.1 网络面冲突 —— Step 2 已经解决，只需标记

这 7 个在 Step 2 的 `git checkout` 之后内容已经是 feature 的，只差 `git add`：

```bash
git add crates/tx-subsystems/src/net/execution/step_device_tx.rs \
        crates/tx-subsystems/src/net/execution/step_process_network_events.rs \
        crates/tx-subsystems/src/net/namespace.rs \
        crates/tx-subsystems/src/net/protocol/tcp.rs \
        crates/tx-subsystems/src/net/protocol/udp.rs \
        crates/tx-subsystems/src/net/structure/payload.rs \
        crates/tx-subsystems/src/net/tests/external_connect_tests.rs
```

最后一个是 **modify/delete 冲突**（main 删了，feature 改了）：**保留 feature 版**。
它是外部 TCP 握手 / 一次 pass 排多段 / ISN 唯一性 / 外部 UDP sendto 的唯一回归网，5 个 test。
`git add` 就等于选择保留。

### 4.2 非网络冲突决策表

| # | 文件 | 取哪边为底 | feature 必须保住什么 | 备注 |
|---|---|---|---|---|
| 1 | `crates/tx-ext4/src/tests_v3.rs` | **main** | `34737cd4` 的 2 行（ext4 容量 8M→256M 的测试期望） | 合并树核对：这 2 行本来就活着，只需在冲突块里保留 |
| 2 | `crates/tx-kernel/src/init.rs` | **main** | `995ee1aa` 1 行 + `2c5fe37b` 5 行 | main 侧 exec/loader 大重写都在这个文件，必须以 main 为底。另需确认 `submit_net_runtime_tasks()` 调用点还在（两侧同名同签名，已核对） |
| 3 | `crates/tx-kernel/src/init/net.rs` | **feature** | 全部（IPv6 boot seed `fec0::15/64`、`34737cd4` 的 FIB 默认路由 26 行、`01aea500`） | **唯一一个以 feature 为底的非纯 net 文件**。要**吸收 main 的一处改动**：`now()` 从 `P::read_ns()` 改成 `timekeeper_clock::<P>().monotonic_now_ns()`（main `init/net.rs:230`）——因为 `wall_clock.rs` 在 main 已删（见 #7） |
| 4 | `crates/tx-kernel/src/thread_future.rs` | **main** | `2c5fe37b` 的 68 行（SEGV post-mortem 增强）✅已核对存活 | ⚠️ `34737cd4` 的 3 行 **syshist**（`syscall_history_snapshot` 调用）在合并树里丢了，见 §4.3 决策 |
| 5 | `crates/tx-shims/src/linux_syscall/fs_basic.rs` | **main** | `995ee1aa` 的 22 行（O_TRUNC 走 `step_truncate`、flush 时 stamp mtime、dup3 fd-drop 补 flush）✅已核对存活 | 这是"echo > tracked 文件 git 看不见"的修复，丢了就回归 |
| 6 | `crates/tx-shims/src/linux_syscall/mod.rs` | **main** | `34737cd4` 32 行 + `2c5fe37b` 29 行 | 4 个冲突块。⚠️ syshist 4 行在合并树里丢了（§4.3）。⚠️ **必须补 `net_set_now_ns` 调用，见 Step 5.1** |
| 7 | `crates/tx-subsystems/src/wall_clock.rs` | **接受 main 的删除** | `995ee1aa` 的 `install_monotonic_ns_source` / `realtime_now_ns_hooked` 需要**移植** | modify/delete 冲突。main 把时间统一迁进新 crate `tx-time` + `tx-services::time`。见 §4.4 |
| 8 | `crates/tx-subsystems/src/vfs/resolution/step.rs` | **main** | `16a066b1` 的 63 行（解析权反转给 FS lookup）✅已核对存活 | "git remote add 丢 url"的真根因修复 |
| 9 | `docs/progress/STATUS.md` | **手工合并** | 双方条目都保留 | 两边都在文件头追加。按时间倒序把 feature 的 5 条和 main 的条目并列 |

### 4.3 决策点：syshist 系统调用历史环

`34737cd4` 在 `mod.rs` + `thread_future.rs` 里加了一个 syscall 历史环形缓冲
（`syshist_record` / `syscall_history_snapshot`，fatal trap 时 dump 最近 28 条），
合并树里这 7 行没活下来（冲突区留了 main 侧）。

- 它是**诊断设施**，不是正确性修复 —— 丢了不会有功能回归
- 但它是当初抓 sa_restorer 那个 bug 的工具

**建议：保留。** 成本是在两个冲突块里手工贴回 7 行；收益是你接下来要修 PR#54 的回退，
这个 dump 在追内核态崩溃时很有用。如果 main 的新 observe 管线（`xtask/src/observe_schema.rs`）
已经覆盖了同样的能力，再删不迟。

### 4.4 决策点：wall_clock.rs 怎么办

事实：
- main **删除** `crates/tx-subsystems/src/wall_clock.rs`，时间统一进新 crate `tx-time`（`tx-services::time` 门面）
- feature 在里面加了 `install_monotonic_ns_source(fn() -> u64)` + `realtime_now_ns_hooked()`
- 唯一消费者：`page_backed/lifecycle.rs:225 let Some(now_ns) = crate::wall_clock::realtime_now_ns_hooked()`
  ——这是 `995ee1aa` 给 writeback 打 mtime 用的，而 **main 的 `page_backed/lifecycle.rs` 里 `mtime` 零命中**
- 存在原因是 page-backed writeback **在 platform 泛型之下**，拿不到 `P: TimeIf` 绑定；
  main 的 `timekeeper_clock::<P>()` 恰恰需要 `P`，**不是改个名就能替换的**

**建议方案：接受 main 的删除，把那个 hook 移植成函数指针。**

1. 在 main 的时间门面里加一个非泛型 hook（`tx-services/src/time/` 下），语义照抄 feature 的：
   boot 时 `install_monotonic_ns_source(|| timekeeper_clock::<P>().realtime_now_ns())`
2. `page_backed/lifecycle.rs:225` 改调新 hook
3. `git rm` 掉 `wall_clock.rs`，同时清掉 feature 侧 10 个引用它的文件里的残留 `use`
   （其中大部分文件本来就取 main 版，main 版已经用 `tx-services::time`，无需改）

**备选（更省事但欠债）**：保留 `wall_clock.rs` 作为薄 shim 只服务这一个调用点。
能过编译，但和 main 的时间分层架构并存，且 main 新增的
`xtask/src/lint_invariants_time_layering.rs`(+1373 行) **可能判它违规** —— 合并后必须实跑
`cargo xtask lint` 确认。

---

## 五、Step 4 — Tier 3：修 12 个编译断裂点

Tier 1 强取 net 之后必然编不过。断裂集中在 3 个文件约 15 处引用。

### 5.1 方向 A：main 的非 net 代码调 feature 没有的 `*_with_post`（8 处，纯机械）

PR#54 给 reactor 加了 post 回调机制（`tx-reactor/src/wait.rs` 的 `fire_with_post` 族），
net 侧所有函数都加了 `_with_post` 后缀。feature 的 net 没有这套签名。
**做法统一：去后缀 + 删掉 post 闭包实参。**

| 文件 | 位置 | 改成 |
|---|---|---|
| `crates/tx-drivers/src/virtio/net.rs` | `:11` import, `:378` | `net_delegate_kick_poll_with_post(&mut post)` → `net_delegate_kick_poll()` |
| `crates/tx-shims/src/linux_syscall/socket.rs` | — | **Step 2 已整树取 feature，此文件无需改** |

> 注意：agent 初版审计把 `socket.rs` 的 6 处 import 也列为断裂点，那是在"socket.rs 归 main"的
> 前提下。本计划 Step 2 把 socket.rs 也取了 feature，所以方向 A **实际只剩 `virtio/net.rs` 一处**。
> 这是本计划与审计文档的一处有意分歧。

### 5.2 方向 B：feature 的 net 调 main 已删的 `wait_source` 旧 API（真成本在这）

main 把注册模型从「`RawQueue`/`RawPort` + `*_with_id`」换成了「`Arc<WaitSource>` + `register_wait_source_with_id`」。
以下符号在 main **已不存在**（我逐个验证过）：`register_wait_queue_with_id`、
`register_wait_port_with_id`、`wait_on_token`、`register_wait_channel`、`release_wait_channel`。

| 文件:行 | 现在调 | 改成 | 难度 |
|---|---|---|---|
| `net/structure/identity.rs:106-108` | `register_wait_queue_with_id(id, RawQueue)` | `register_wait_source_with_id(id, Arc<WaitSource>)` | **真适配**，要把 `RawQueue` 包成 `WaitSource`，不是改名 |
| `net/structure/identity.rs:109` | `register_wait_port_with_id(id, RawPort)` | 同上 | 同上 |
| `net/delegate/runtime.rs:134,165` | `wait_source::wait_on_token(token)` | `wait_on_registered_source_id(token.source_id(), token.interest())` | 中 |
| `net/facade/driver.rs:48-53` | `wait_source::wait_on_token` | 照抄 main 的 `net/notification.rs::registered_wait_for_yield(shape)` | 中 |
| `net/tests/checks_bind_tests.rs:403,493` | `wait_on_token` | 同上（test profile） | 低 |

**关键提示**：main 的 `wait_source.rs` 的 `REGISTRY` 是**统一注册表**，同时装
`WaitSource`/`RawQueue`/`RawPort` 三种变体，`wait_on_registered_source_id` 三种都能解析。
所以方向 B 未必要把 `RawQueue` 真改成 `WaitSource` —— **先试最省的路**：
保留 `register_wait_queue`（main 有），再**额外**调一次
`register_wait_source_with_id(id, wait_routing::new_wait_source(id))` 把 substrate 镜像补上
（这正是 feature `net/adapter.rs::install_substrate_mirrors` 在做的事）。
这样既编得过，又保住了 R4a 修复。

### 5.3 保留 `net/notification.rs`

Step 2.1 保留了 main 新增的这个文件。它是"notification 汇聚屋"：
`xtask/src/lint_invariants_notification.rs` 里 `MAX_NOTIFICATION_BOUNDARY_VIOLATIONS = 0`，
只有 `*/adapter.rs` 和 `*/notification.rs` 允许直接碰 `wait_source::*` / `YieldShape::OnWaitSource`。

feature 的 `net/delegate/runtime.rs` 和 `net/facade/driver.rs` 直接调 `wait_on_token`，
**会让 `cargo xtask lint` 变红**。把这些调用收进 `net/notification.rs` 即可。
注意 feature 侧还有 `net/adapter.rs`（main 删了、我们保留），它也是合法的 convergence home。

---

## 六、Step 5 — 两条非编译但必修（否则合完是"能编译的坏网络"）

### 6.1 net 时钟桥没人喂 ⚠️

feature 的 `linux_syscall/mod.rs:984` 有：
```rust
tx_subsystems::net::clock::net_set_now_ns(P::read_ns());
```
main 版 `mod.rs` **没有这个调用**（全树 0 命中）。Step 3 的 #6 以 main 为底，
这行会消失 → feature 的 net `clock.rs` 时钟桥在 syscall 路径上读不到新时间，
只能靠 delegate（`net/delegate/runtime.rs:193`）刷新。

**必须在解 #6 冲突时手工补回这一行。** 补回后按 main 的时间门面调整取值来源
（`P::read_ns()` 是否仍可用，还是要走 `timekeeper_clock::<P>()`）。

### 6.2 R4a / epoll-on-socket

已实证：main 上 `epoll_pwait(timeout=-1)` 对已 bind 的 socket **立即返回 0**，feature 正确停泊。
Step 2 强取 feature 的 net 会把 `net/adapter.rs` + `install_substrate_mirrors` 带回来，
理论上自动修复。**用探针验证**：

```bash
git apply .v6work/epoll-socket-r4a-probe.patch
cargo test -p tx-shims --lib -- --test-threads=1 dispatch_epoll_pwait_blocks_until
# 期望 2 passed
```

**建议合并后把这个测试留在树里**——它是目前唯一能红/绿区分这个洞的回归网，
而且全树原本零个 epoll+socket 测试，这就是 PR#54 能溜过去的原因。

---

## 七、Step 6 — 验证门（分级，前一级不过不要往下走）

### 门 1：编译（两架构）

```bash
cargo xtask full-build --target rv64-qemu --skip-doctor --no-image
cargo xtask full-build --target la64-qemu --skip-doctor --no-image
```
判据：`full-build: ok` + 零新增 warning。

### 门 2：lint（PR#54 新增了 10 个架构 lint，这是新增门）

```bash
cargo xtask lint
```
重点看 `lint_invariants_notification`（ratchet=0）和 `lint_invariants_time_layering`。
**如果 §4.4 选了保留 `wall_clock.rs` 的备选方案，这一门大概率会红。**

### 门 3：单测集合差

⚠️ **基线选择变了**：审计证明 main 的网络能力本身是回退态，
所以"相对 main 无 net 回归"**对网络部分不再是有意义的判据**。分开算：

```bash
# 网络部分 → 基线用 feature（premerge-feature-20260727）
# 非网络部分 → 基线用 main（这才是上次翻车的地方）
cargo test -p tx-subsystems --lib -- --test-threads=1 > /tmp/merged-unit.raw 2>&1
grep -a "^failures:$" -A 100000 /tmp/merged-unit.raw | grep -aE "^    [a-z]" | sort -u > /tmp/merged-unit.failures
diff .v6work/unit-baseline.failures /tmp/merged-unit.failures
```

feature 带来的 4 个新测试在全量跑里会因模块级共享态级联计入失败，**必须隔离复跑**：
```bash
cargo test -p tx-subsystems --lib -- --test-threads=1 net::tests::external_connect_tests   # 期望 8/8
cargo test -p tx-subsystems --lib -- --test-threads=1 net::tests::rtnetlink_tests          # 期望 25/25
```

### 门 4：R4a 探针 + vendored smoltcp

```bash
git apply .v6work/epoll-socket-r4a-probe.patch
cargo test -p tx-shims --lib -- --test-threads=1 dispatch_epoll_pwait_blocks_until   # 2 passed

cd external/smoltcp-asterinas
RUSTFLAGS="--cap-lints allow" cargo test \
  --features "medium-ethernet,medium-ip,proto-ipv4,proto-ipv6,socket-tcp,socket-udp,socket-raw" --lib
# 期望 591 passed / 0 failed
```

### 门 5：端到端

```bash
bash tools/verify-git-net.sh          # 期望 8 passed 0 failed
for s in netperf-musl iperf-musl netperf-glibc iperf-glibc; do
  bash .v6work/suite-run.sh $s rv target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt merged
  bash .v6work/suite-run.sh $s la target/loongarch64-unknown-none-softfloat/debug/tx-kernel-loongarch64-qemu-virt merged
done
# 期望每架构 22/22。iperf REVERSE_UDP 偶发 Connection refused 是已知 flake,重跑即过,但报告里要写明。
```

### 门 6：LTP 全量对账（四 lane，别省）

```bash
bash .v6work/ltp-verdicts.sh <log>
bash .v6work/ltp-reconcile.sh <module>...
```
**非网络模块基线用 main**（上次 86 个 la.musl 回归就是漏了这个）；
**网络模块基线用 premerge-feature-20260727**。

---

## 八、Step 7 — 提交

```bash
git add -A
git commit          # 不 rebase、不改写已有 commit,保留 merge commit
```

commit message 里逐条说明冲突取舍（按第四节的表）。**不要加 coauthor。**

提交前做 progress catch-up：更新 `docs/progress/STATUS.md`（改了什么/跑了什么验证/下一步/阻塞），
改动的 JSON 用 `cargo xtask progress validate` 校验。

---

## 九、harness 坑（踩过的，会让你得出错误结论）

1. **`tx.oscomp=<suite>` 不是选择器**，只有 `tx.oscomp.groups=<suite>` 是。写错会静默跑默认组列表，
   而 judge 照样打印一个看起来合理的分数（实测 414/449 vs 真实 5/5）。
2. **必须加 `tx.oscomp.observe=0`**，否则 bench-observe 中途 dump + `system_off` 把结果截断成假回归。
   `cargo xtask oscomp test` 注入不了这个参数，**别用它**。
3. **永远 boot 镜像副本**。`oscomp test` 直接挂共享的 4GB `sdcard-<arch>.img`，一次中途 `system_off`
   就把 ext4 block bitmap 弄不一致。已知 `sdcard-rv.img` 目前就是这个状态。
4. **改完源码必须重新编译再验证**。`cargo test` 不重建内核 ELF，只跑单测就去 QEMU 会验到旧内核。
   每次 QEMU 验证前 `stat -c '%y %n' <改过的源文件> <kernel ELF>` 确认。
5. **不要并行跑多个 QEMU。**
6. **怀疑回归时用 `KERNEL_DIR=` 换基线内核在同一 harness 复跑**，而不是比较两次不同配置的结果。
7. **epoll-on-socket 探针必须先 bind**。裸 socket 没有活协议引擎，拿不到 wait token，
   在 `sources.is_empty()` 就提前返回 0 —— 两个分支都会 FAIL，会让你误判成"不是回归"。

---

## 十、工作量估计

| 阶段 | 估时 | 风险 |
|---|---|---|
| Step 0-2（准备 + Tier 1 强取） | 20 分钟 | 低，纯机械 |
| Step 3（9 个非网络冲突） | 1-2 小时 | 中，#2/#6 冲突块多 |
| Step 4（编译断裂点） | 1-2 小时 | 方向 B 的 `identity.rs` 是唯一真设计工作 |
| Step 5（两条必修） | 30 分钟 | 低 |
| Step 6（验证门 1-4） | 1-2 小时 | 门 2 lint 是新增的未知数 |
| 门 5-6（端到端 + LTP 四 lane） | 数小时机时 | 别省 |

合计 **0.5-1 人日**手工 + 一轮完整回归机时。
