# Wait 注册表统一 · Stage 2 风险裁定 + 分阶段安全顺序

<!-- txdoc:07-NET-WAIT-STAGE2-V1 -->

> 阶段来源：Wait 统一进度记忆（`wait-registry-unification`）+ [`NET_AUDIT_v1.md`](NET_AUDIT_v1.md) 审计 ⑥ / R4a。Stage 1（net 阻塞 recv/send/accept/connect 迁 substrate WaitSource，commit `c40cba5b`，0 回归）已落。**本文=纯调研零改码**，只出风险裁定 + 六阶段安全顺序。所有 file:line 按 `c40cba5b`。

---

## 0. 结论速览（先看这个）

| 块 | 风险 | 一句话 |
|---|---|---|
| **① 枢纽：ppoll/pselect/select → `await_any_wait_source`** | **低-中** | epoll 就是现成模板。唯一雷 = **tty**（epoll 从没经 substrate 消费过它）。 |
| **② 删 reactor `Channel`** | **目标错位 —— 不能删** | `Channel` 承重 `Completion`/`wait_event`/超时等待（boot + net-delegate timer）。Stage 2 删的是 **tx-subsystems `wait_source.rs` 桥**，不是 reactor `Channel`。 |
| **③ 11 子系统去双注册 + tx-subsystems 消费者** | **中** | 5 个机械、6 个需当心。硬约束：**先迁所有消费者、再撤任何生产者**。vm/net-delegate 需 ~30 LOC 新基建；net facade driver 是死码。 |

**最可能弄坏的地方**：(1) tty 的 ppoll 唤醒走 substrate 路径（从没被真消费过 —— epoll 无 tty 臂）；(2) 撤掉某子系统的 `fire_legacy_channel` 时若该载体仍有消费者停在 `wait_on_token` → **静默丢唤醒（挂死，不是编译错）**。顺序纪律就是全部。

---

## 1. 架构回顾 —— 3 原语、2 注册表

wait/wake 有三个底层载体，**底层同构**（都往 `TaskMailbox` 投 `MailboxEvent::SourceFired`、都带 generation + `InterestMask`）：

1. reactor `Channel` = `{ port: RawPort, timers: Option<TimerQueue> }` —— [`wait.rs:107`](../../../crates/tx-reactor/src/wait.rs)。既是遗留 fd 载体，**又是** `wait_event`/`Completion` 的底座。
2. bus `RawQueue`/`RawPort` —— `crates/tx-substrate/src/bus/`（电平 / 边沿）。
3. substrate `WaitSource` —— [`wake/wait_source.rs:83`](../../../crates/tx-substrate/src/wake/wait_source.rs)，**目标**，带 `pending_mask` 防丢唤醒锁存 + `prepare`/`install_if`（比 1、2 结构性更安全）。

三者只在**用哪个注册表解析 id**上分叉：

- **遗留注册表**（tx-subsystems）：[`wait_source.rs:60`](../../../crates/tx-subsystems/src/wait_source.rs) `static REGISTRY: BTreeMap<u64, RegisteredWaitSource{Channel|RawQueue|RawPort}>`。消费入口 = `wait_on_token(WaitToken) -> RegisteredWaitFuture`（`:183`）。
- **substrate 注册表**（tx-substrate）：`wake/wait_source.rs:444` `BTreeMap<u64, Arc<WaitSource>>`；`lookup_source`（`:475`）、`register_source`（`:455`）。消费入口 = tx-shims `await_wait_source`/`await_any_wait_source`（[`linux_syscall/wait.rs:22/33`](../../../crates/tx-shims/src/linux_syscall/wait.rs)）。

**"双注册"** = 子系统铸一个 id，在遗留表登一个 `Channel`/`RawQueue`/`RawPort`（`register_wait_*_with_id`）**并且**在 substrate 表登一个 `WaitSource`（`new_wait_source(id)`），`notify_*` 时**两边都 fire**（`fire_legacy_channel` + `notify_v3_source`）。范式样板 = [`eventfd/notification.rs:39-74`](../../../crates/tx-subsystems/src/eventfd/notification.rs)。

**已就位的护栏**：[`xtask/lint_invariants_wait.rs`](../../../xtask/src/lint_invariants_wait.rs) 已 ratchet `MAX_LEGACY_WAIT_CHANNEL_SITES = 0`（现存 site 挂 skip-list）。此 lint 是**盟友** —— skip-list 逐步缩、最终真零。

---

## 2. 块① 枢纽：ppoll/pselect/select

`sys_ppoll` 与 `sys_pselect6`（[`io.rs`](../../../crates/tx-shims/src/linux_syscall/io.rs)，等待处 `:1288` / `:1654`）同一形态：

```
构建 wait_tokens: Vec<WaitToken>            // 每个阻塞 fd 一个 WaitToken::new(source_id, interest)
futures = wait_tokens.filter_map(wait_on_token)   // 遗留表解析
wait_on_any_token[_or_pselect_deadline](futures, mailbox)   // io.rs:435 / :465
```

**模板就是 `epoll.rs`**（`sys_epoll_wait_until:535`、`wait_for_epoll_wake:329`）：

```
构建 sources: Vec<(WaitSourceId, InterestMask)>  // wait_sources_for_entries:313
await_any_wait_source(ctx, sources)               // substrate 解析，io.rs 已 race timer
```

可行性高，因为**两条路的 per-fd source_id accessor 完全一样**。ppoll/pselect 从 `tfd.source_id()`、`rx.reader_source_id()`、`tx.writer_source_id()`、`socket_poll_wait_token_from_file(...).source_id()`、`tty.wait_source_id()`、`efd.reader/writer_source_id()`、`payload.reader/writer_source_id()` 推 token（io.rs:1066-1226、1431-1609）；epoll 的 `epoll_wait_source`（`epoll.rs:91`）把**同样**的 accessor 映射成 `WaitSourceId`。这些 id 全部双注册（见块③），故 `lookup_source` 都能解析。

信号 EINTR + 超时模板已含：`await_any_wait_source` 遇 `MailboxEvent::SignalDelivered` 即返回（wait.rs:114/136），`wait_for_epoll_wake` 用 `poll_fn` race `timer_sleep::sleep_until_ns` —— 正是 io.rs `wait_on_any_token_or_pselect_deadline` 已在用的机械。醒来后重扫 fd → 都不就绪且有 pending 信号 → EINTR（io.rs 已在做 `thread_pending_signal_interrupts`）。

**失败模式**

- **tty（雷）**：epoll 的 `epoll_wait_source`/`supports_epoll`（epoll.rs:91-190）**没有 tty 臂**。所以 tty 的 substrate `WaitSource` + 其 RX 上的 `notify_v3_source` **从没有过真订阅者** —— ppoll 迁移会是它的首航。若 tty 的 substrate notify 路径有潜伏缺陷（UART-IRQ 驱动的唤醒），ppoll 在 tty 上永久阻塞。**不是编译错**。这是首要单独验证项。
- **interest 粒度**：遗留 `push_unique_wait_token` 按 `(source_id, interest)` 去重；epoll 的 `wait_sources_for_entries` 按 source 去重、用 `InterestMask::new(u64::MAX)`。过度唤醒（wake-on-any + 重扫）POSIX 安全（重扫才是真相源），但要有意识地选、别撞上。
- **mailbox 模型切换**：遗留下每个 `RegisteredWaitFuture` 自持一个新 `TaskMailbox`（wait_source.rs:195），信号单独在 `ctx.mailbox` 上盯；substrate 下 `await_any_wait_source` 把**所有** source 用一个 generation 登在共享 `ctx.mailbox` —— 更干净、已被 epoll 证过。低风险但是真行为变化，要盯一眼。

**裁定：不是雷，但爆炸半径大**（每个 poll/select 用户）。作为独立一步做，用 tty + 多 fd 混合验证。

---

## 3. 块② reactor `Channel`：能删吗？

**不能。这个目标错位了。** [`wait.rs:107`](../../../crates/tx-reactor/src/wait.rs) 的 `Channel` **机制上**是纯 wait 载体（其自身注释："publication surface, not truth source"），但**用途上承重**，且与 fd 注册表无关：

- [`completion.rs`](../../../crates/tx-reactor/src/completion.rs) —— `Completion` + 计数完成建在 `Channel` + `wait_event`（WaitProtocol）上。
- [`sync_coord.rs`](../../../crates/tx-reactor/src/sync_coord.rs) —— 协调原语持一个 `Channel`。
- [`runtime.rs`](../../../crates/tx-reactor/src/runtime.rs) —— `Reactor::channel()` / `declared_channel*` 发超时-连线的 channel。
- **`wait_event` 家族的活跃超时消费者**：[`net/delegate/timer.rs`](../../../crates/tx-subsystems/src/net/delegate/timer.rs) `net_delegate_wait_tick_deadline`（`Channel::wait_event(WaitProtocol::InterruptibleTimeout(..))`）、boot [`init.rs`](../../../crates/tx-kernel/src/init.rs) + `init/net.rs`（`InterruptibleTimeout(deadline)`）。

substrate `await_wait_source` **没有**超时/中断分类 —— 那套 richness 只在 reactor `wait_event`（+ `WaitProtocol`，它在 [`tx-reactor/wait.rs:39`](../../../crates/tx-reactor/src/wait.rs) **和** [`tx-substrate/step/wait_protocol.rs:15`](../../../crates/tx-substrate/src/step/wait_protocol.rs) **各定义一份**；reactor 的 Completion/net-delegate/boot 用 reactor 那份）。把这套搬上 substrate 是**另一个更大的 epic**，不属"退遗留 wait 注册表"。

**Stage 2 从 Channel 侧真正撤掉的**：
- 各子系统的 `register_wait_channel_with_id` 登记；
- 各子系统的 `fire_legacy_channel` 双通知 + reactor `fire_legacy` helper（`wait.rs:258`）；
- tx-subsystems 遗留 `REGISTRY` + `wait_on_token` + `RegisteredWaitFuture`/`RawQueueWaitFuture`/`RawPortWaitFuture`（`wait_source.rs` 整文件）。

之后 `Channel` **留着** —— 不再是 wait 注册表参与者，仍是 Completion/超时的底座。**把 Stage 2 目标改名**：从"删 reactor Channel"改成"删 tx-subsystems `wait_source.rs` 桥"。

---

## 4. 块③ 撤 11 子系统双注册 + tx-subsystems 消费者

### 4a. 遗留表的消费者（必须全部先迁）

真 `wait_on_token(` 调用点（非测试）：

| 站点 | crate | 能否从 substrate 触达 | 备注 |
|---|---|---|---|
| `io.rs:1288` ppoll | tx-shims | ✅ `await_any_wait_source` | 块① |
| `io.rs:1654` pselect | tx-shims | ✅ | 块① |
| `signal.rs:606` rt_sigtimedwait | tx-shims | ✅ `await_wait_source`（有 `ctx.mailbox`） | 随 shim 批次迁 |
| `socket/helpers.rs:1811` `SocketReadyWait::Legacy` | tx-shims | ✅（Stage-1 fallback，已 substrate 优先） | 删 fallback 臂 |
| `vm/execution.rs:1367` `await_range_lock` | **tx-subsystems** | ⚠️ 需新基建 | range_lock **已**自持 `Arc<WaitSource>`（`range_lock.rs:117`） |
| `net/delegate/runtime.rs:134/165` delegate loop | **tx-subsystems** | ⚠️ 需新基建 | queue **无** substrate 镜像（见 4c） |
| `net/facade/driver.rs:53` `wait_on_yield_shape` | tx-subsystems | — | **死码**：唯一 caller `drive_socket_connect_waiting` 零调用点 |

### 4b. tx-subsystems 消费者的新基建

tx-subsystems 调不了 tx-shims `await_wait_source`，**但** `lookup_source`/`WaitSource::register` 在 tx-substrate 是 `pub`（tx-subsystems 依赖 tx-substrate）。所需 helper 很小、自足（`MailboxSourceFuture` 的镜像，但**自造一个新 `TaskMailbox`** 而非取 `ctx.mailbox`）：

```rust
// ~30 LOC，放 tx-subsystems（或上提 tx-substrate）
async fn await_source(src: &Arc<WaitSource>, interests: InterestMask) {
    let mbox = Arc::new(TaskMailbox::new());
    let gen = mbox.next_generation();
    let sub = src.register(Arc::downgrade(&mbox), gen, interests);
    park_until_source_fired(&mbox, gen).await;   // 形同 RawQueueWaitFuture
    src.unregister(sub);
}
```

- **vm range_lock**：直接传它自持的 `&Arc<WaitSource>`（无需 lookup）。不可中断内核锁 → 无信号集成需求 → 新 mailbox 够。
- **net delegate**：内核背景任务、无信号 → 新 mailbox 够。但其载体是裸 `RawQueue`、无 substrate 镜像（4c）—— 要么补镜像，要么直接 await 该 RawQueue（`RawQueueWaitFuture` 需要的是 queue 而非 REGISTRY，故把 queue 直接交给它即可在 REGISTRY 删除后存活）。

此基建风险：**低**（与现有 `RawQueueWaitFuture` 同构；substrate `WaitSource` 是更安全的载体）。

### 4c. 生产者 —— 机械度梯度（子代理测绘 + eventfd 范式亲验）

- **Canonical、机械**（删 `register_wait_channel_with_id` + `fire_legacy_channel`，保 `notify_v3_source`）：**eventfd、pipe、timerfd、tty、vfs**。
- **直接 `.fire()`（非 `fire_legacy_channel`）—— 删法略异但简单**：signalfd、ipc_msg、ipc_sem。
- **futex** —— notify 用 hint 感知的 `source.notify_with_hint`/`notify_limit_emit_with_hint`；substrate 侧已对，删 Channel 半边即可。
- **process** —— **三路** fire：Channel + substrate `WaitSource` + 一个 `exit_source_bus` RawQueue（structure.rs ~910-931）。删 Channel 前先验 bus 订阅者。
- **net（socket）** —— **非 Channel 化**：`register_wait_queue_with_id` ×3（recv/send/accept wq）+ `register_wait_port_with_id` ×1（urgent），经 `notify_mirror` 镜像到 substrate（identity.rs:100-123、readiness.rs:78-102）。Stage 1 已把 socket 阻塞路由到 substrate 镜像；这里撤掉遗留表 `_with_id` 登记 + RawQueue/RawPort 作为 `wait_on_token` 目标的角色。**面最大、耦合最深。**
- **net delegate** —— `queue.rs:26` 用裸 `register_wait_queue`（**无 `_with_id`、无 substrate 镜像**）。独一份：需补镜像，或直接-queue await（4b）。

**硬不变式**：对每个载体，*只有当它的消费者不再经 `wait_on_token` 解析后，才撤掉该生产者的遗留 notify*。反序 = 静默丢唤醒。

---

## 5. 分阶段安全顺序（每步可独立验证 + 可回退）

排序原则：**消费者先于生产者**；**一次一个载体族**；在配对消费者完全迁走前保留双通知。

- **P0 —— 基建，零行为变化。** 加 tx-subsystems `await_source(&Arc<WaitSource>, interests)`（4b）。暂无 call site。*验证*：rv64+la64 编译；tx-subsystems 集合差 = 基线。
- **P1 —— 删死码。** 删 `net/facade/driver.rs::wait_on_yield_shape` + `drive_socket_connect_waiting`（零 caller）+ 其 re-export。*验证*：编译；集合差 = 基线。
- **P2 —— shim 消费者迁 substrate。** `signal.rs:606`（rt_sigtimedwait）+ `socket/helpers.rs:1811`（删 Legacy fallback）改 `await_wait_source`。*验证*：git-net 8/8；LTP net rv.musl 0 回归；sigtimedwait 冒烟。
- **P3 —— 枢纽（隔离）。** ppoll/pselect/select（io.rs）迁 `await_any_wait_source` + timer race，仿 `wait_for_epoll_wake`。source-id 去重按 (id,interest) 或有意识改用 u64::MAX。*验证*：**单独跑 tty ppoll（那个雷）**、多 fd 混合（socket+pipe+timerfd+eventfd）、poll 超时、信号 EINTR；git-net 8/8；LTP `poll*/pselect*/select*/ppoll*` + `epoll*` 无回归。
- **P4 —— tx-subsystems 消费者。** vm `await_range_lock` → `await_source(range_lock.wait_source, RANGE_LOCK_RELEASE_MASK)`。net delegate loop → substrate（补镜像**或**直接-queue await）。*验证*：range-lock 争用冒烟；net delegate 仍泵（loopback + 外部 TCP 冒烟，tcp/udp/dns）；集合差。
- **P5 —— 逐族撤生产者遗留半边。** 依赖安全序（某载体只在其消费者全上 substrate 后）：canonical 五 → 直接-fire 三 → futex → process（验 bus 订阅）→ net socket（撤 `_with_id`）→ net delegate。每子系统 = 独立 commit + 集合差。`notify_v3_source` 处处保留。
- **P6 —— 删桥。** 删 `crates/tx-subsystems/src/wait_source.rs`（REGISTRY、`wait_on_token`、`RegisteredWaitFuture`/`RawQueue|RawPortWaitFuture`、`register_wait_*`）、`fire_legacy`/`fire_legacy_channel`、io.rs `wait_on_any_token*` helper。把 `lint_invariants_wait.rs` skip-list 缩到空（ceiling 已 0）。*验证*：rv64+la64 完整 full-build；git-net 8/8；**完整 LTP net + syscalls parity**（poll/select/epoll/eventfd/timerfd/signalfd/pipe/tty/ipc/futex 族）；集合差。

**明确不在 Stage 2**：删 tx-reactor `Channel`/`wait.rs`/`wait_event`/`Completion`（块②）。若哪天要做，另立 epic。

---

## 6. 诚实结论 —— 哪里最可能弄坏 + 怎么防

1. **tty 走 substrate 首航（最高）。** epoll 从没经 substrate 表驱动过 tty，故 P3 是 tty 的 `notify_v3_source` 处女航。**防**：信任批次前先专项 tty-ppoll 隔离测试；若挂，锅在 tty 的 notify 路径、不在 io.rs。
2. **顺序性丢唤醒（静默）。** 在消费者仍调 `wait_on_token` 时撤 `fire_legacy_channel`，编译干净、运行挂死。**防**：严格消费者先行；双通知留到 P5；每载体一 commit + runtime 冒烟，P5 绝不 big-bang。
3. **net socket + net delegate（耦合）。** socket 是 RawQueue/RawPort 非 Channel；delegate 干脆无镜像。**防**：当作 P5 最后、最严审的子步；socket 靠 Stage-1 已证的 substrate 路径；delegate 优先直接-queue await（新面最小）。
4. **误删 Channel。** 想删 reactor `Channel` 会炸 Completion/boot/net-timer。**防**：目标就是 tx-subsystems 桥，句号。

**验证纪律**（据 Wait 统一进度记忆）：别信宿主 `cargo check` —— `target/debug` 被 1.94-nightly 工具链污染（`tx-fat`/`itoa`/`memchr` 假错）；项目钉 nightly-2025-05-20。真门 = `cargo xtask full-build --target rv64-qemu` + `tools/verify-git-net.sh` +（`cargo xtask oscomp submit --target rv64-qemu` 后 `KERNEL_DIR=target/oscomp/submit tools/ltp-runtest-witness.sh`）。回归判定 = tx-subsystems lib-test **集合差** vs 基线，不看绝对数。
