# 网络栈重构 P0 执行计划 v1：解冻时钟 + 常驻 Interface 骨架

<!-- txdoc:07-NET-P0-V1 -->

**Status.** v1.1 (2026-07-02)。[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md) 阶段 **P0** 的可执行细化。属"下半·引擎"战线第一步。§6 设计点已拍板 **A（全局 `NET_NOW_NS`）**，四处改动已实施并按 §7 验证记录通过全部四层验证。

**Purpose.** 把 smoltcp TCP 状态机的**冻结时钟**解开——这是审计 [`NET_AUDIT_v1.md`](NET_AUDIT_v1.md) ① + R2a 的总根。P0 只做"解冻"，**不碰** 5 缓冲（P1）、loopback 直拷（P1）、外部 TCP（P2）、per-netns Interface（P5）。

**基准.** 工作树 `feature-network-refactor @ fd64ba24`。所有 `file:line` 按此 HEAD；smoltcp fork 在 `external/smoltcp-asterinas`。

**给现场赛的读者.** 本文按"**为什么冻 → 原来怎样 → 改成怎样 → 为什么安全 → 怎么测**"组织，每条事实都带行号可自查。目标是让你能脱离大模型、独立复现这条推理与实现。

---

## 0. 一句话

<!-- txdoc:07-NET-P0-V1-ONELINE -->

> smoltcp 的 TCP 状态机（`Box<tcp::Socket>`）干活时要读 `cx.now()`，但给它 `cx` 的 `with_context`（`tcp.rs:712-720`）**每次新造一个 `Interface(Instant::ZERO)`**，时钟恒为 0 → 重传/RTT/TIME-WAIT 定时器全死。P0 = 引入全局时钟桥 `NET_NOW_NS`（有 `P: TimeIf` 的 delegate/syscall 写入真实时间，P-无关的 `with_context` 读出），并把一次性 Interface 换成**常驻 Interface**、用前戳上真实 `now`。

---

## 1. 病根：时钟冻在哪（先把机制讲透）

<!-- txdoc:07-NET-P0-V1-DIAG -->

### 1.1 事实链（每条可 grep 复核）

**① `RawTcpSocket` 内部就是 smoltcp 的 socket**（不是绕过）。`protocol/tcp.rs:24-33`：

```rust
pub struct RawTcpSocket {
    socket: SpinMutex<Box<tcp::Socket<'static>>>,   // ← smoltcp TCP 状态机本体（握手/重传/RTT 归它）
    protocol_state: SpinMutex<RawTcpProtocolState>,
    last_syn_ack: SpinMutex<Option<SmoltcpTcpSegment>>,
    rx_buffer: SpinMutex<VecDeque<u8>>,             // ┐
    tx_buffer: SpinMutex<VecDeque<u8>>,             // ├ 旁挂 5 缓冲 = 审计 ②（双通路），P0 不碰
    corked_tx: SpinMutex<Vec<u8>>,                  // ┘
    recv_capacity: usize,
    send_capacity: usize,
}
```

**② smoltcp 干活要读 `cx.now()`。** `external/smoltcp-asterinas/src/socket/tcp.rs`：

```
:287   fn should_retransmit(&self, timestamp: Instant) -> Option<Duration>   // 该不该重传
:1439  if cx.now() < self.challenge_ack_timer { ... }
:1711  self.rtte.on_ack(cx.now(), ack_number);                              // RTT 估计
:1785  self.timer.set_for_idle(cx.now(), self.keep_alive);                  // keepalive / TIME_WAIT
```

`now` 来自传给 `process/dispatch/connect` 的 `cx: &mut Context`。

**③ `cx` 由 `with_context` 造，`now` 写死为 0。** `protocol/tcp.rs:712-720`：

```rust
fn with_context<R>(f: impl FnOnce(&mut smoltcp::iface::Context) -> R) -> R {
    let mut device = Loopback::new(Medium::Ip);
    let mut iface = Interface::new(
        Config::new(HardwareAddress::Ip),
        &mut device,
        smoltcp::time::Instant::ZERO,   // ← 每次都新造 Interface，时钟恒 0
    );
    f(iface.context())                  // ← 把 now=0 的 cx 交给 smoltcp
}
```

三个调用点全在这条路上：`connect_endpoint`（`tcp.rs:408`）、`dispatch_segment`（`tcp.rs:428`）、`process_segment`（`tcp.rs:449`）。smoltcp 每次都以为"现在是 0 时刻"，重传定时器永不到期 → **无重传、无 RTT、无 TIME-WAIT 老化** = 审计 ① + R2a。

### 1.2 反直觉实证：真实时间已算出，只是没接上

- **delegate 生产路径早有真实时间**：`delegate/runtime.rs:206` 把 `driver.now()` 传进 step 函数。
- `driver.now()` 是真的（`init/net.rs:227-229`）：

  ```rust
  fn now(&self) -> Instant {
      let micros = P::read_ns() / 1_000;   // ← P::read_ns() = HAL 单调时钟（纳秒）
      Instant::from_micros(micros.min(i64::MAX as u64) as i64)
  }
  ```

- **但真实时间传到 `PollContext.timestamp`（`poll_context.rs:21-27`）就断了，从没接进 `with_context`**——后者无视一切、自造 0 时钟。

### 1.3 为什么当初会断（也是 D3 的由来）

`with_context` 在 `tx-subsystems`（step 层），这层 **P-无关**（无 `P: TimeIf` 泛型），`P::read_ns()`（`wall_clock.rs:128` 的 `monotonic_now_ns::<P>` 也需 `P`）和 `driver.now()` 都够不着 → 当初就地填了 `ZERO`。**解法（v2 §3 D3）**：用一个**不带泛型的全局 `AtomicU64`** 当桥——有 `P` 的层（delegate/syscall）写入真实时间，无 `P` 的 `with_context` 读出。

### 1.4 fork 已备好设 `now` 的接口（无需改 fork）

`external/smoltcp-asterinas/src/iface/interface/mod.rs`：

```
:126  pub struct InterfaceInner { ... }
:128      pub now: Instant,                            // now 是 pub 字段
:275  pub fn context(&mut self) -> &mut InterfaceInner // = smoltcp::iface::Context
:799  pub fn set_now(&mut self, now: Instant)          // 亦可用它
```

所以"解冻"只需持有一个 Interface、用前把 `.now` 写成真实时间。

---

## 2. 目标改动（四处，外科手术）

<!-- txdoc:07-NET-P0-V1-CHANGES -->

P0 只让 smoltcp 拿到真实 `now`；顺带把"一次性 Interface"换成"常驻 Interface"当骨架（对 loopback 是**零行为变化**，但 P2 外部 TCP 要靠它）。

### 改动 1：新建时钟桥 `NET_NOW_NS`

放 `tx-subsystems`（所有相关 crate 都依赖它）。新文件 `crates/tx-subsystems/src/net/clock.rs`：

```rust
use core::sync::atomic::{AtomicU64, Ordering};
use smoltcp::time::Instant;

/// 真实单调时钟（纳秒）。由有 `P: TimeIf` 的层（delegate/syscall）写入，
/// 供 P-无关的 step/socket 层读取。见 REFACTOR_PLAN_A_v2 §3 D3。
pub static NET_NOW_NS: AtomicU64 = AtomicU64::new(0);

pub fn net_set_now_ns(ns: u64) {
    NET_NOW_NS.store(ns, Ordering::Relaxed);   // 源本身单调，Relaxed 足够
}

pub fn net_now_instant() -> Instant {
    Instant::from_micros((NET_NOW_NS.load(Ordering::Relaxed) / 1_000) as i64)
}
```

在 `net` 模块导出（`net/mod.rs` 加 `pub mod clock;`）。

### 改动 2：`with_context` —— 常驻 Interface + 每次戳真实 `now`

`protocol/tcp.rs:712`，整个函数替换：

```rust
use crate::net::clock::net_now_instant;

// 单 netns 骨架：一个常驻 Interface，只当 Context 提供者（校验和能力 + now + 邻居）。
// 多 netns 时改 per-netns（P5）。
static CONTEXT_IFACE: SpinMutex<Option<Interface>> = SpinMutex::new(None);

fn with_context<R>(f: impl FnOnce(&mut smoltcp::iface::Context) -> R) -> R {
    let mut slot = CONTEXT_IFACE.lock();
    let iface = slot.get_or_insert_with(|| {
        let mut device = Loopback::new(Medium::Ip);
        Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut device,
            smoltcp::time::Instant::ZERO,
        )
    });
    let cx = iface.context();
    cx.now = net_now_instant();   // ← 解冻：smoltcp 读 cx.now() 前戳上真实时间
    f(cx)
}
```

> - `Interface::new` 只在构造时 `&mut device` 借用一次，构造完不再持有 device，故常驻的只需 `Interface`。
> - 若 `SpinMutex::new` 非 `const fn`，用一次性 `Once`/懒初始化包一层。

### 改动 3：delegate 每步把真实时间写进桥（必须）

`delegate/runtime.rs:186` `net_delegate_step_once` 开头加一行：

```rust
pub fn net_delegate_step_once(driver: &dyn NetDelegateDriver, guard: &Guard<'_>) -> NetDelegateRuntimeOutcome {
    crate::net::clock::net_set_now_ns((driver.now().total_micros().max(0) as u64) * 1_000);  // ← 新增
    // ... 原有逻辑不动 ...
}
```

poll 路径的 `process/dispatch` 都在本函数内发生 → 在函数**开头**写，`with_context` 随后即读到新鲜值。

### 改动 4：socket syscall 入口也写一次（加固 inline connect）

`tx-shims` 的 socket 相关 syscall（connect/send/recv/accept/poll）入口，`P` 在作用域内：

```rust
tx_subsystems::net::clock::net_set_now_ns(P::read_ns());
```

> 改动 3 **必须**（poll 路径的重传/老化靠它）；改动 4 **加固**（inline connect 时钟更准）。P0 可先只做 3，连通后再补 4。

---

## 3. 为什么这样改是安全的（关键性质）

<!-- txdoc:07-NET-P0-V1-SAFETY -->

**`NET_NOW_NS` 默认 0；任何不写它的代码，`with_context` 读到的仍是 0 = 与现状完全一致。** 只有 delegate（生产）和 syscall 写它 → 只有生产路径解冻。推论：

- **绝大多数现有单测**（自己传 `PollContext::new(Instant::ZERO)`、不碰 `NET_NOW_NS`）行为**完全不变**。
- **常驻 Interface 对 loopback 是零行为变化**：loopback 是 `Medium::Ip`，不走 ARP/邻居/路由，Interface 里跨调用累积的状态没被用到；唯一变量是 `now`，且被 `NET_NOW_NS` 门控。故 P0 对 loopback 测试应**全绿不变**。
- **需盯的一个点**：`delegate_*` 用假 driver 的测试——改动 3 会把它们 driver 的 `now()` 写进桥。若某测试 driver 返回非零 `now` 却断言"冻结行为"，会红。这**不是 bug**，是该测试在断言旧的错误行为，按解冻后预期更新即可。

**已知代价（可接受，P3 解决）**：`CONTEXT_IFACE` 是一把全局锁，把所有 socket 的 process/dispatch/connect 串行化。锁序统一（`CONTEXT_IFACE` 外、`socket` 内；三个调用点一致 → **不死锁**），只是并行度下降。对 loopback/LTP 无碍；D4 本就要"每 iface 一把锁"，P3 收敛。

---

## 4. 测试方案（四层，从快到全）

<!-- txdoc:07-NET-P0-V1-TEST -->

### 第 1 层 — 桥接单测（证明管道通）

```rust
net_set_now_ns(1_500_000_000);   // 1.5s in ns
assert_eq!(net_now_instant(), Instant::from_micros(1_500_000));
```

### 第 2 层 — 判决性单测："重传证明时钟活了"（P0 的灵魂测试）

放 `net/tests/` 下，直接打 `RawTcpSocket`：

```rust
net_set_now_ns(0);
let sock = RawTcpSocket::new(&SocketOptionSet::default());
sock.connect_endpoint(local, remote).unwrap();      // 进入 SynSent

let syn1 = sock.dispatch_segment();                 // 第一次：发 SYN
assert!(syn1.is_some(), "首个 SYN 应发出");

let syn2 = sock.dispatch_segment();                 // 立刻再发：没到 RTO，应无输出
assert!(syn2.is_none(), "时间没走，不该重传");

net_set_now_ns(2_000_000_000);                      // +2s，越过初始 RTO(~1s)
let syn3 = sock.dispatch_segment();                 // 时间走了 → 应重传 SYN
assert!(syn3.is_some(), "时钟解冻后，超时应重传 SYN");
```

**旧代码**：`now` 恒 0，`syn3` 永远 `None` → 红。**新代码**：`syn3` 有值 → 绿。这一条精确锁死 P0 成败。

> 注意：`NET_NOW_NS` / `CONTEXT_IFACE` 是全局态，此测试应独立跑或留意与其他测试的先后，避免并行串扰。

### 第 3 层 — QEMU 观测：真实启动里 `now` 在走

临时在 `net_delegate_step_once` 里加带节流的 `tx_klog` 打印 `NET_NOW_NS`，`cargo xtask test busybox-boot` 跑一个 loopback TCP，`grep` 串口日志确认值**递增**（非恒 0），验完删除打印。

### 第 4 层 — 不回归门（必须过）

- `cargo -q xtask unit` 全绿（host 全套）。
- QEMU 跑 loopback TCP/UDP 的 LTP 子集，对照 P0 前**不退化**。

---

## 5. 验收 / 提交 / 范围

<!-- txdoc:07-NET-P0-V1-DONE -->

- **验收**：第 2 层测试红→绿；第 1、4 层全绿；第 3 层观测到 `now` 递增。
- **提交**（粗粒度）：P0 = 一个 commit，`net: P0 解冻时钟 — 常驻 context Interface + NET_NOW_NS`。
- **明确不做**：删 5 缓冲（P1）、loopback 直拷（P1）、外部 TCP（P2）、per-netns Interface（P5）。P0 只解冻。

---

## 6. 待确认的设计点：全局桥 vs 显式穿参

<!-- txdoc:07-NET-P0-V1-DECISION -->

| 方案 | 做法 | 取舍 |
| ---- | ---- | ---- |
| **A. 全局 `NET_NOW_NS`（推荐，= v2 D3，本文按此写）** | 加全局桥，只改 `with_context` 一处，调用点签名全不动 | 改动最小、最好实现；代价是引入一个全局可变量（但只是单调时钟提示，弱一致无害） |
| B. 显式穿参 | 给 `process_segment(…, now)`/`dispatch_segment(…, now)` 加 `now` 参数，从 `PollContext.timestamp` 传下去 | 数据流显式、更"干净"、好测；但要改每个调用点签名，且 `connect` 的 syscall inline 路径没有 `PollContext`，仍需另找时钟 → 反而更碎 |

**推荐 A**：P0 要小、要稳、要能手写，少动几处更不易错；B 的"显式"优点留到 P3 socket 收敛时一并拿到更合适。**若改选 B，本文 §2 改动 2/3/4 需相应重写。**

> **拍板（2026-07-02）：用户确认 A。** 实施与验证结果见 §7。

---

## 7. 验证记录（2026-07-02，A 方案实施完成）

<!-- txdoc:07-NET-P0-V1-VERIFIED -->

四处改动落点（与 §2 一致）：

| 改动 | 文件 | 内容 |
| ---- | ---- | ---- |
| 1 | `crates/tx-subsystems/src/net/clock.rs`（新建）+ `net/mod.rs` | `NET_NOW_NS` / `net_set_now_ns` / `net_now_instant` |
| 2 | `net/protocol/tcp.rs` `with_context` | 常驻 `static CONTEXT_IFACE: SpinMutex<Option<Interface>>` + 每次 `cx.now = net_now_instant()` |
| 3 | `net/delegate/runtime.rs` `net_delegate_step_once` 开头 | `net_set_now_ns(driver.now() → ns)`（负值截 0、饱和乘） |
| 4 | `tx-shims/linux_syscall/mod.rs` `dispatch_inner` | `syscall_publishes_net_clock(nr)`（connect/send*/recv*/accept*/shutdown/setsockopt/ppoll/pselect6）→ `net_set_now_ns(P::read_ns())` |

四层验证结果（全过）：

1. **桥接单测** `net::tests::clock_tests::net_clock_bridge_converts_ns_to_micros`：绿。
2. **判决性单测** `tcp_syn_retransmits_after_clock_advance`：新代码绿；**红判已实测**——临时把 `cx.now` 钉回 `ZERO` 后，恰在 `syn3` 断言处失败，证明该测试精确区分新旧行为。（`--test-threads=1` 串行，全局态无串扰；测试尾部复位 `NET_NOW_NS=0`。）
3. **QEMU 观测**：`init=/bin/tcp-loopback-smoke` 启动 rv64-qemu，临时打印显示 `NET_NOW_NS` 跨 delegate 步 **588807500 → 611921000 → 641242000 ns 单调递增**（非恒 0）；且首个 delegate 步之前桥已非零 = 改动 4 的 syscall 钩子先行写入。打印已删除。
4. **不回归门**：
   - `tx-subsystems --lib` 全量：816 过 / 305 败，失败集合与无 P0 基线**逐条相同**（`comm` 集合差为空）——305 败为分支既有（bridge/namespace 系，见 progress 记忆），P0 零新增失败，+2 过 = 新增时钟测试。
   - `cargo -q xtask unit`：tx-kernel 75 过、tx-scripts 56 过；tx-shims（测试模块编译错）与 tx-ext4（truncate ENOSYS 断言）两处失败经 stash 对照**均为分支既有**。
   - QEMU：`busybox-boot` 哨兵 ok；`tcp-loopback-smoke` → `tx-n68-tcp-ok`；`udp-loopback-smoke` → `tx-n68-udp-ok`（UDP 无 delegate 打印，符合"UDP 回环全内联"的既知架构）。

盯点确认：§3 预告的 `delegate_*` 假 driver 测试无一变红（多数测试 driver 返回 `Instant::ZERO` → 桥写 0 = 原行为，印证"默认 0 安全"性质）。

---

## 附录 A. 证据锚点

<!-- txdoc:07-NET-P0-V1-EVIDENCE -->

**当前代码（`feature-network-refactor @ fd64ba24`）**：
- 冻结点：`crates/tx-subsystems/src/net/protocol/tcp.rs:712-720`（`with_context` 一次性 `Interface(ZERO)`）。
- 三个调用点：`tcp.rs:408`（connect_endpoint）、`tcp.rs:428`（dispatch_segment）、`tcp.rs:449`（process_segment）。
- socket 本体与 5 缓冲：`tcp.rs:24-33`。
- 真实时间已在 delegate：`delegate/runtime.rs:206`（传 `driver.now()`）、`delegate/runtime.rs:186`（`net_delegate_step_once`）。
- `driver.now()` 实现：`crates/tx-kernel/src/init/net.rs:227-229`。
- P-无关断点上游：`protocol/poll_context.rs:21-27`（`PollContext{timestamp,…}`）、`:452`（`created_at = timestamp`）、`:498`（`process_segment` 调用）。
- P-泛型时钟：`crates/tx-subsystems/src/wall_clock.rs:128`（`monotonic_now_ns::<P: TimeIf>`）。

**smoltcp fork（`external/smoltcp-asterinas`）**：
- 读 `cx.now()`：`src/socket/tcp.rs:287/1439/1711/1785`。
- 设 `now` 接口：`src/iface/interface/mod.rs:128`（`pub now`）、`:275`（`context()`）、`:799`（`set_now`）。

**关联**：[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md)（§3 D1–D4/D3、§5 P0）、[`NET_AUDIT_v1.md`](NET_AUDIT_v1.md)（① 根因、R2a 老化失效）。

---

*P0 只解冻时钟；解冻后重传/RTT/TIME-WAIT 复活，为 P1（loopback 恢复 smoltcp 委托 + 瘦 socket）铺路。所有 `file:line` 按 `fd64ba24`。*
