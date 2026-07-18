# P0 改动逐文件讲解（教学向）：解冻时钟到底改了哪 8 个文件

<!-- txdoc:07-NET-P0-WALKTHROUGH-V1 -->

**Status.** v1 (2026-07-02)。对应提交 `5ab58517`（`net: P0 解冻时钟 — 常驻 context Interface + NET_NOW_NS 时钟桥`）。本文是 [`REFACTOR_P0_v1.md`](REFACTOR_P0_v1.md) 的实施配套读物：那篇讲"为什么这么设计"，本文讲"最终到底改了什么、每处长什么样"，给现场赛脱离大模型复现用。

**读法.** 先看 §0 整体图景，再按 §1→§5 的顺序读（桥 → 读端 → 两个写端），测试放最后。§9 有改动后的完整代码流程图。

---

## 0. 整体图景：一座桥

<!-- txdoc:07-NET-P0-WALKTHROUGH-V1-OVERVIEW -->

整个 P0 只做一件事：**给 smoltcp 的 TCP 状态机接上真实时钟**。

原来的病：smoltcp 判断"该不该重传 / RTT 多少 / TIME-WAIT 到期没"全靠调用方传进来的 `cx.now()`，而给它 `cx` 的 `with_context` 位于 `tx-subsystems` 的 step 层——这一层**没有 `P: TimeIf` 平台泛型**，够不着硬件时钟 `P::read_ns()`，当初就地填了 `Instant::ZERO`。于是 smoltcp 永远以为"现在是 0 时刻"，所有定时器死透。

解法是架一座桥：

> **一个不带泛型的全局原子变量 `NET_NOW_NS`——"有 P 的层"往里写真实时间，"无 P 的层"从里读。**

8 个文件按角色分组：

| 角色                     | 文件                                                  | 改动量           |
| ------------------------ | ----------------------------------------------------- | ---------------- |
| 桥本身                   | `crates/tx-subsystems/src/net/clock.rs`             | 新建 29 行       |
| 挂模块                   | `crates/tx-subsystems/src/net/mod.rs`               | +1 行            |
| **读端**（病根处） | `crates/tx-subsystems/src/net/protocol/tcp.rs`      | +26/−7          |
| 写端①：网络轮询循环     | `crates/tx-subsystems/src/net/delegate/runtime.rs`  | +9               |
| 写端②：syscall 入口     | `crates/tx-shims/src/linux_syscall/mod.rs`          | +23              |
| 测试                     | `crates/tx-subsystems/src/net/tests/clock_tests.rs` | 新建 47 行       |
| 挂测试模块               | `crates/tx-subsystems/src/net/tests.rs`             | +1 行            |
| 进度记录                 | `docs/progress/STATUS.md`                           | +89 行（纯文档） |

---

## 1. `net/clock.rs`（新建）—— 桥本身

<!-- txdoc:07-NET-P0-WALKTHROUGH-V1-CLOCK -->

```rust
pub static NET_NOW_NS: AtomicU64 = AtomicU64::new(0);   // 全局时钟，纳秒，默认 0

pub fn net_set_now_ns(ns: u64) {                        // 写端调用
    NET_NOW_NS.store(ns, Ordering::Relaxed);
}

pub fn net_now_instant() -> Instant {                   // 读端调用，转 smoltcp 的时间类型
    Instant::from_micros((NET_NOW_NS.load(Ordering::Relaxed) / 1_000) as i64)
}
```

三个要点：

- **默认 0 是刻意的安全带**：不写桥的代码（比如绝大多数现有单测）读到的还是 0，行为与改动前完全一致。只有生产路径（delegate/syscall）解冻。
- **`Relaxed` 就够**：源时钟本身单调，桥只是"新鲜度提示"，不需要跟别的内存操作定序。
- **纳秒存、微秒出**：内核 HAL 时钟是纳秒（`P::read_ns()`），smoltcp 的 `Instant` 是微秒精度，除以 1000 在读端做。

## 2. `net/mod.rs`（+1 行）—— 挂模块

```rust
pub mod clock;
```

让新文件成为 `net` 模块的一部分。`pub` 是必须的：写端②在**另一个 crate**（`tx-shims`），要通过 `tx_subsystems::net::clock::net_set_now_ns(...)` 跨 crate 调用。

## 3. `protocol/tcp.rs` —— 读端（病根处，核心改动）

<!-- txdoc:07-NET-P0-WALKTHROUGH-V1-READER -->

改动前（每次调用新造一个 Interface，时钟写死 0）：

```rust
fn with_context<R>(f: impl FnOnce(&mut smoltcp::iface::Context) -> R) -> R {
    let mut device = Loopback::new(Medium::Ip);
    let mut iface = Interface::new(
        Config::new(HardwareAddress::Ip),
        &mut device,
        smoltcp::time::Instant::ZERO,   // ← 恒 0，用完即弃
    );
    f(iface.context())
}
```

改动后（常驻 Interface + 每次进入戳真实时间）：

```rust
use crate::net::clock::net_now_instant;

// 单 netns 骨架：一个常驻 Interface，只当 Context 提供者。锁序统一
// CONTEXT_IFACE 外、self.socket 内。多 netns 是 P5。
static CONTEXT_IFACE: SpinMutex<Option<Interface>> = SpinMutex::new(None);

fn with_context<R>(f: impl FnOnce(&mut smoltcp::iface::Context) -> R) -> R {
    let mut slot = CONTEXT_IFACE.lock();
    let iface = slot.get_or_insert_with(|| {          // 首次用到才构造，之后复用
        let mut device = Loopback::new(Medium::Ip);
        Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut device,
            smoltcp::time::Instant::ZERO,
        )
    });
    let cx = iface.context();
    cx.now = net_now_instant();                        // ← 解冻：从桥里读真实时间
    f(cx)
}
```

两个变化，各自的意义：

1. **`Interface` 从"用完即弃"变成常驻**（`static` + 懒初始化）。对当前 loopback 是零行为变化（`Medium::Ip` 不走 ARP/邻居，跨调用没有被用到的累积状态），但 P2 做外部 TCP 时要靠这个常驻骨架。
2. **`cx.now` 从恒 0 变成读桥**。smoltcp 的 `should_retransmit` / `rtte.on_ack` / `set_for_idle`（重传、RTT、keepalive/TIME-WAIT）全读这个值，这一行就是"解冻"本身。

三个调用点（`connect_endpoint` / `dispatch_segment` / `process_segment`）**一行没改**——它们本来就都通过 `with_context` 拿 `cx`，桥接在函数内部完成。这正是选 A 方案（全局桥）而不是 B 方案（显式穿参）的收益：改一处，调用点签名全不动。

已知代价（接受，P3 收敛）：`CONTEXT_IFACE` 是一把全局锁，所有 socket 的 process/dispatch/connect 串行化。锁序三个调用点一致（先 `CONTEXT_IFACE` 后 `socket`），不会死锁，只是并行度下降；对 loopback/LTP 无碍。

## 4. `delegate/runtime.rs`（+9 行）—— 写端①：网络轮询循环

<!-- txdoc:07-NET-P0-WALKTHROUGH-V1-WRITER1 -->

`net_delegate_step_once` 是网络后台任务每一步的入口。它手里**早就有真实时间**（`driver.now()`，由 `init/net.rs` 的 `BootNetDelegateDriver` 用 `P::read_ns()` 实现）——以前只是没人把它传到 `with_context`。在函数开头加：

```rust
pub fn net_delegate_step_once(
    driver: &dyn NetDelegateDriver,
    guard: &Guard<'_>,
) -> NetDelegateRuntimeOutcome {
    // 先写桥、再干活：本步内所有走到 with_context 的代码都读到新鲜时间。
    crate::net::clock::net_set_now_ns(
        u64::try_from(driver.now().total_micros())
            .unwrap_or(0)              // Instant 是 i64 微秒，负值截成 0
            .saturating_mul(1_000),    // 微秒→纳秒，饱和乘挡溢出
    );

    let ready = net_delegate_queue().peek();
    // ……原有逻辑一字未动……
}
```

为什么放开头：poll 路径的收包（`process_segment`）、发包（`dispatch_segment`）都发生在本函数内部，开头写一次，整步共享。重传/老化的解冻**主要靠这一处**（改动⑤是加固）。

## 5. `tx-shims/linux_syscall/mod.rs`（+23 行）—— 写端②：syscall 入口

<!-- txdoc:07-NET-P0-WALKTHROUGH-V1-WRITER2 -->

有些网络路径不经过后台任务——最典型的是 `connect`，SYN 是在 syscall 里内联发出的。所以在 syscall 分发器加一个钩子：

```rust
/// socket 族 syscall 可能内联驱动 smoltcp（connect 发 SYN、send/recv、
/// shutdown 发 FIN、setsockopt 冲刷 cork），入口处把真实时钟发布进桥，
/// 让 with_context 在这些路径上也读到新鲜时间，而不只靠 delegate 步。
fn syscall_publishes_net_clock(nr: u64) -> bool {
    nr == NR_CONNECT
        || nr == NR_SENDTO   || nr == NR_RECVFROM
        || nr == NR_SENDMSG  || nr == NR_RECVMSG
        || nr == NR_SENDMMSG || nr == NR_RECVMMSG
        || nr == NR_ACCEPT   || nr == NR_ACCEPT4
        || nr == NR_SHUTDOWN || nr == NR_SETSOCKOPT
        || nr == NR_PPOLL    || nr == NR_PSELECT6 || nr == NR_PSELECT6_TIME64
}

async fn dispatch_inner<'a, P: PmapIf + EntropyIf + TimeIf + ...>(
    req: SyscallRequest,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    if syscall_publishes_net_clock(req.nr) {
        tx_subsystems::net::clock::net_set_now_ns(P::read_ns());   // 这层有 P
    }
    // ……原有 match 分发一字未动……
}
```

只对 socket 族的 14 个 syscall 号写桥，其余 syscall 完全不受影响。成本是一次 CSR 读 + 一次原子写，可忽略。QEMU 实测里它比 delegate 更早写入（首个 delegate 步之前桥已非零），证明钩子生效。

## 6. `net/tests/clock_tests.rs`（新建）—— 两个测试

<!-- txdoc:07-NET-P0-WALKTHROUGH-V1-TESTS -->

**测试①（桥接）**：写 1.5 秒（纳秒），读出来应是 1_500_000 微秒。证明管道通、单位换算对。

**测试②（判决性，P0 的灵魂）**：

```rust
net_set_now_ns(0);
let socket = RawTcpSocket::new(&SocketOptionSet::default_tcp());
socket.connect_endpoint(endpoint(4055), endpoint(4056)).unwrap();  // 进 SynSent

let syn1 = socket.dispatch_segment();
assert!(syn1.is_some());     // 第一次：发出 SYN

let syn2 = socket.dispatch_segment();
assert!(syn2.is_none());     // 立刻再发：时间没走，没到 RTO，不该重传

net_set_now_ns(2_000_000_000);            // 拨到 2s，越过初始 RTO(≈700ms)
let syn3 = socket.dispatch_segment();
assert!(syn3.is_some());     // 时间走了 → 必须重传 SYN

net_set_now_ns(0);           // 复位全局态，不污染后续测试
```

为什么它是"判决性"的：旧代码时间恒 0，`syn3` 永远 `None`（**红判亲测**：临时把 `cx.now` 钉回 `ZERO`，测试恰在此断言失败）；新代码绿。一个测试精确锁死"时钟活没活"，且三步分别对应 smoltcp dispatch 的三条分支（发新段 / 定时器未到期返回 / `should_retransmit` 触发重发）。

前提依据（fork `external/smoltcp-asterinas/src/socket/tcp.rs`）：发出段后 `set_for_retransmit(cx.now(), rto)` 挂定时器（:2535）；`should_retransmit` 只在 `now >= expires_at` 时触发（:289）；初始 RTO = RTT 300ms + 4×偏差 100ms ≈ 700ms（:145-146）。

**并发注意**：`NET_NOW_NS`/`CONTEXT_IFACE` 是进程级全局态，本仓库单测统一 `--test-threads=1` 串行（`xtask/src/unit.rs:24`、CI 同），无跨测试串扰；测试尾部仍复位为 0 作卫生习惯。

## 7. `net/tests.rs`（+1 行）—— 挂测试模块

```rust
mod clock_tests;
```

与 §2 同理。子测试文件用 `use super::*;` 复用 `tests.rs` 顶部的公共导入和 `endpoint()` 等辅助函数（仓库测试惯例）。

## 8. `docs/progress/STATUS.md`（+89 行）—— 进度记录

项目规矩：每完成一件事记录改了什么、怎么验证、下一步、有无阻塞。纯文档，对代码无影响。验证结论的正式版在 [`REFACTOR_P0_v1.md`](REFACTOR_P0_v1.md) §7。

---

## 9. 改动后的代码流程图

<!-- txdoc:07-NET-P0-WALKTHROUGH-V1-FLOW -->

### 9.1 全局数据流：谁写桥、谁读桥

```
                 用户程序 (busybox / LTP / smoke bin)
                            │
                            │ 系统调用 (ecall)
                            ▼
        ┌─────────────────────────────────────────┐
        │ tx-shims  dispatch_inner   【有 P】       │
        │                                          │
        │  if syscall_publishes_net_clock(nr) {    │
        │      net_set_now_ns(P::read_ns()) ───────┼──┐    改动⑤ 写端②
        │  }                                       │  │
        │  match nr { NR_CONNECT => sys_connect…}  │  │
        └───────────────┬──────────────────────────┘  │
                        │ 内联路径                      │
                        │ (connect 发 SYN / send /      │
                        │  recv / shutdown 发 FIN)      │
                        ▼                              ▼
        ┌──────────────────────────┐      ╔══════════════════════╗
        │ step_connect / step_send │      ║  NET_NOW_NS          ║
        │ … (tx-subsystems step 层) │      ║  AtomicU64 (纳秒)     ║
        └───────────────┬──────────┘      ║  net/clock.rs  改动①  ║
                        │                 ╚══════════╦═══════════╝
                        │                       ▲    ║
   reactor 后台任务       │                       │    ║ 读
        │               │                       │写   ║
        ▼               │                       │    ║
┌───────────────────────┴──────────┐            │    ║
│ net_delegate_step_once  【有 P】   │            │    ║
│                                   │            │    ║
│  net_set_now_ns(driver.now()) ────┼────────────┘    ║   改动④ 写端①
│  step_process_network_events…     │  (driver.now()  ║
│  step_process_loopback_pending…   │   = P::read_ns) ║
└───────────────┬───────────────────┘                 ║
                │ 收包/发包                             ║
                ▼                                     ▼
        ┌────────────────────────────────────────────────────┐
        │ protocol/tcp.rs  RawTcpSocket   【无 P——所以要桥】    │
        │                                                     │
        │  connect_endpoint / dispatch_segment /               │
        │  process_segment                                     │
        │        │                                             │
        │        ▼                                             │
        │  with_context {                          改动②③ 读端 │
        │      lock CONTEXT_IFACE (常驻 Interface, 懒初始化)     │
        │      cx.now = net_now_instant()   ◀── 从桥读，解冻     │
        │      f(cx)                                           │
        │  }                                                   │
        └───────────────┬─────────────────────────────────────┘
                        │ cx（带真实 now）
                        ▼
        ┌─────────────────────────────────────────┐
        │ smoltcp tcp::Socket（fork）               │
        │   socket.connect(cx,…)  发 SYN            │
        │   socket.dispatch(cx,…) should_retransmit(cx.now()) │
        │   socket.process(cx,…)  rtte.on_ack(cx.now())        │
        │   → 重传 / RTT估计 / keepalive / TIME-WAIT 全部复活    │
        └─────────────────────────────────────────┘
```

### 9.2 时间线实例：一次 SYN 重传是怎么发生的（= 判决性测试的三步）

```
 时间(桥里的值)      动作                                     smoltcp 内部
 ──────────────    ────────────────────────────────────    ─────────────────────────────
 t=0            ①  connect_endpoint()                      state = SynSent
                    └ with_context: cx.now=0

 t=0            ②  dispatch_segment()  → Some(SYN)         发出 SYN；
                    └ with_context: cx.now=0                set_for_retransmit(0, ~700ms)
                                                            定时器: expires_at ≈ 700ms

 t=0            ③  dispatch_segment()  → None              should_retransmit(0)?
                    └ cx.now=0 < 700ms                       0 < 700ms → 不触发，无输出
                                                            【旧代码永远停在这一步】

                ④  net_set_now_ns(2_000_000_000)           （桥被写端拨到 2s；
                                                             生产环境=时间自然流逝）

 t=2s           ⑤  dispatch_segment()  → Some(SYN)         should_retransmit(2000ms)?
                    └ cx.now=2s ≥ 700ms                      2000 ≥ 700 → 触发！
                                                            回卷 remote_last_seq，重发 SYN
```

旧代码卡死在③：`cx.now` 永远是 0，定时器永远"没到期"，丢了的 SYN/数据段永远不会重发——这就是审计①（无真重传）和 R2a（老化全失效）的机制。P0 之后，只要写端持续把真实时间灌进桥，⑤就会自然发生。

---

*本文与 [`REFACTOR_P0_v1.md`](REFACTOR_P0_v1.md)（设计+验证记录）、[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md)（总方案）配套。对应提交 `5ab58517`；行号引用 fork 为 `external/smoltcp-asterinas`。*
