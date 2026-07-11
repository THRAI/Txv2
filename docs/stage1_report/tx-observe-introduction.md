# tx-observe 介绍：txKernel 的内核侧观测运行时

> 面向开发者 / 评审的介绍稿。代码引用取自当前工作树真实实现，并标注实现 / 脚手架 /
> 仅文档三种状态。规范文档：`docs/Txv3/08_OBSERVATION_v1.md`（架构）、
> `08_OBSERVATION_SERIALIZATION_v0.md`（线格式）、`08_OBSERVATION_HOST_v0.md`（主机侧）。

## 1. 它是什么

`tx-observe` 是 txKernel 的**内核侧观测运行时**。一句话定位：

> 每个 hart（CPU 核）独占一个无锁、零分配的 emitter，把 span / 事件 / 计数写进一段
> per-hart SPSC 环形缓冲；主机侧的 `tx-trace-daemon` 把环里的定长记录解码成
> Perfetto / JSON，重建出 syscall → drive → step 的嵌套时间线。

它解决的问题是：**在不打断内核执行、不引入锁与分配的前提下，得到一条结构化、可在
Perfetto 里直接查看的全栈执行轨迹。** 你写一个 syscall 处理函数，什么埋点都不加，就
自动获得一条带参数标注的 trace 切片（见 §6）。

设计上它刻意只做"内核 → 环"这一段，与下面这些解耦（`OBS-V1-CLEAVAGE-1`）：

- 不是 `RawTrace` / `ConsoleIf` 那种行式日志；
- 不负责 Perfetto 协议生成（那是主机侧 daemon 的事）；
- 不依赖板级传输细节（环的物理承载由各板的 `ObserverIf` 提供）。

---

## 2. crate 形态

| 项 | 事实 | 锚点 |
|---|---|---|
| 主 crate | `tx-observe`，`#![no_std]`，无 allocator | `crates/tx-observe/src/lib.rs:1` |
| 伴随类型 crate | `tx-observe-types`，no_std；可选 `host` feature 加 serde（供 daemon） | `crates/tx-observe-types/` |
| 依赖 | 仅 `tx-hal` + `tx-observe-types`，极薄 | `crates/tx-observe/Cargo.toml` |
| feature | `testing` — 暴露 `tx_observe::testing`（用 alloc-backed buffer + 自旋测试锁） | `Cargo.toml:13` |

主 crate 的模块结构：

```
crates/tx-observe/src/
├── lib.rs        核心 emitter 运行时:HartEmitter、环生产者、init、dump
├── encode.rs     payload 字节编码(OBS-3a,全 #[inline]、栈上 [u8;16] 缓冲)
├── macros.rs     traced_syscall! 宏 —— L0 边界埋点
├── hart_local.rs per-hart 存储(HartLocalArray)
└── testing.rs    TestPlatform / TestObservation RAII 测试夹具

crates/tx-observe-types/src/
├── header.rs     TxTraceHeader / TxTraceHartRing(环头 + 游标)
├── record.rs     TxTraceRecord / TxTraceKind / TxTraceLevel
└── payload.rs    TxPayloadTag + 所有 16 字节 Payload* 结构
```

---

## 3. 不变量：为什么它能"白嫖"进热路径

`tx-observe` 的卖点不是功能多，而是**代价可控到能放进每个 syscall**
（`lib.rs` 头部 §Invariants）：

- **OBS-1** 每条 emit 路径 O(1) 且有界；
- **OBS-3** 零分配（`#![no_std]`，无 allocator）；
- **OBS-4** 永不阻塞——生产者路径无 mutex、无 CAS；
- **OBS-5** 溢出有损——满了就把 `ring.lost` 计数 +1（Relaxed）立即返回，绝不背压；
- **OBS-9** span id 在 `(hart, boot)` 内唯一——高 8 位是 hart_id，低 56 位是本地计数器。

反模式 **OBS-A-1**：禁止在 `StepOp::poll` 内部 emit（会污染 drive 的语义路径），
埋点只允许在约定的"汇聚点"（convergence point，见 §5）。

---

## 4. 核心抽象

### 4.1 `HartEmitter` —— per-hart 门面

通过 `tx_observe::current() -> Option<&'static HartEmitter>` 拿到当前 hart 的 emitter
（`lib.rs:789`）。所有方法都是 `&self`——per-hart 独占，因此无需加锁
（`lib.rs:360` 起）：

```rust
pub fn span_begin(&self, level, name, parent, payload_tag, payload) -> SpanId  // 开 span
pub fn span_end(&self, span, payload_tag, payload)                            // 关 span
pub fn instant(&self, level, name, parent, payload_tag, payload)              // 瞬时点事件
pub fn counter(&self, name: EventNameId, value: i64)                          // 计数采样
pub fn allocation(&self, track: AllocationTrack, name, value: u64)            // 分配诊断标记
pub fn lock_metric(&self, lock, metric, value: u64)                           // 锁计时标记
```

### 4.2 `SpanId` / `EventNameId`

```rust
pub struct SpanId(u64);       // 高8位 hart_id | 低56位本地单调计数;0 = NONE 哨兵
pub struct EventNameId(u32);  // 事件名标识,与 daemon 侧名表对齐
```

事件名用编译期 FNV-1a 哈希生成，`const fn` 保证名字在编译期稳定，且与
`tx-scripts::drive` 里的 op-name 哈希共用同一哈希空间（`lib.rs:80`）：

```rust
pub const fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;          // FNV offset basis
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(0x0100_0193); // FNV prime
        i += 1;
    }
    hash
}
```

### 4.3 父 span 透传

L0（syscall）开的 span 要成为 L2（drive）/L4（step）的父节点。靠一对 per-hart 静态
访问器把父 span 顺着调用链传下去，免得每个 `step` 都得手动穿参数（`lib.rs:145`）：

```rust
pub fn set_current_parent_span(span: SpanId) -> SpanId   // 返回旧值,供调用方恢复
pub fn current_parent_span() -> SpanId
```

---

## 5. 事件模型与分层

### 5.1 记录类型 `TxTraceKind`

`SpanBegin(10)` / `SpanEnd(11)` / `Instant(12)` / `Counter(13)`、
`TrackTombstone(14)`（对象回收）、`PanicMarker(31)`、`ArgContinuation(40)`（大参数续传）
等（`record.rs:131`）。

### 5.2 八个 trace level `TxTraceLevel`（`record.rs:178`）

| Level | 名称 | 含义 | 汇聚点 |
|---|---|---|---|
| **L0** | Boundary | U/K 边界（syscall 进/出） | `tx-shims` 的 `traced_syscall!` 宏 |
| L1 | Script | 脚本级事件（MVP 暂缓） | — |
| **L2** | Drive | `drive<O>` 循环进/出 | `tx-scripts::drive` |
| L3 | Yield | yield / resume 分类 + 唤醒通知流 | drive |
| **L4** | Step | `StepOp::step()` 调用与结果 | drive 的 step 调用点前后 |
| L5 | Phase | substrate 级阶段转换（BSP/AP init） | （待 RawTrace 订阅接线） |
| L6 | Mutation | zone/index 变更、分配/锁度量 | 同上 |
| L7 | Sched | reactor 任务上 hart 的调度（OBS-9） | reactor |

MVP 范围是 **L0/L2/L3/L4**（`OBS-V1-LEVELS-MVP-PROGRESSIVE §7`）；L1 暂缓，L5/L6 等
`RawTrace<P>` 订阅接线。

### 5.3 一条 `sys_read` 的嵌套轨迹（来自规范 §15.4）

```
SpanBegin L0 sys_read    payload=SyscallEnter{sysno=READ, abi=LinuxRv64, argc=3}
  SpanBegin L2 PipeReadOp payload=DriveBegin{mode=Waiting, interrupt=Interruptible}
    SpanBegin L4 step.iteration_0
    SpanEnd   L4           payload=StepOutcome{variant=Yield, shape_kind=OnWaitSource}
    SpanBegin L4 step.iteration_1
    SpanEnd   L4           payload=StepOutcome{variant=Done, progress_value=4096}
  SpanEnd   L2 PipeReadOp
SpanEnd   L0 sys_read     payload=SyscallExit{result_kind=Ok, ret=4096}
```

这正好对应 `docs/stage1_report/syscall-template-sys_read.md` 里讲的
L0→L2→L4 父子链——`read` 是状态机，每次 `step` 的 yield 形态都被记录下来。

---

## 6. 线格式与编码

### 6.1 定长记录 `TxTraceRecord` —— 80 字节，原生字节序（`record.rs:9`）

```
偏移  0  magic        u16 = 0x5254 (b"TR")
偏移  3  kind         u8   (TxTraceKind)
偏移  4  level        u8   (TxTraceLevel)
偏移  6  arg_count    u8   (ArgContinuation 数量)
偏移 16  seq          u64  (per-hart 单调记录序号)
偏移 24  ts           u64  (trace-clock 时间戳)
偏移 32  span         u64  (span id;0 = orphan)
偏移 40  parent       u64  (父 span id;0 = 无)
偏移 48  name         u32  (EventNameId)
偏移 52  payload_tag  u16  (TxPayloadTag)
偏移 54  payload_len  u16
偏移 56  payload      [u8;16]  (定长内联缓冲)
偏移 72  _pad3        [u8;8]   (补齐到 80)
```

`size_of::<TxTraceRecord>() == 80` 由编译期断言钉死（`record.rs:14`）。

### 6.2 编码规则（`encode.rs`，对应 OBS-2 / OBS-13）

- **不用 bytemuck**，逐字段写进栈上 16 字节缓冲；
- **零分配**，所有缓冲都是 `[u8; 16]`；
- 内核侧不带 `Debug` / `Serialize`，主机侧靠 `tx-observe-types` 的 `host` feature 开
  serde。

每个编码函数都返回 `([u8; 16], u16)`（缓冲 + 实际长度）。payload 结构全部 `#[repr(C)]`
且 ≤16 字节，例如：

```rust
PayloadSyscallEnter { sysno: u32, abi: u16, argc: u16 }                       // 8B
PayloadSyscallExit  { ret: i64, errno: i32, result_kind: u8, _pad: [u8;3] }   // 16B
PayloadDriveBegin   { op_type: u32, mode: u8, interrupt: u8, has_deadline: u8,
                      _pad: u8, task_id_low: u32 }                            // 12B
PayloadStepOutcome  { variant: u8, progress_empty: u8, progress_kind: u8,
                      shape_kind: u8, errno: i32, progress_value: u32, _pad }  // 16B
```

### 6.3 环 `TxTraceHartRing`（`header.rs:84`）

208 字节 per-hart 环头（含 `AtomicU64` 的 producer/consumer/seq/lost 游标，因带原子故
**不**标 `Pod`），其后是 2 的幂个 80 字节记录槽。**SPSC**：生产者是 hart，消费者是主机
daemon。

---

## 7. 谁在用它（消费方）

内核侧主要汇聚点（grep `tx_observe::` 跨 workspace）：

- **tx-shims**：syscall 派发入口 `dispatch`（`crates/tx-shims/src/linux_syscall/mod.rs:517`）
  在调用每个 `sys_*` 前后自动开/关 L0 span，并为寄存器参数 a0–a5 各发一条 `ArgValue`
  记录。**这就是"白嫖 trace"的来源——处理函数零改动即获得带参数的切片。**
- **tx-scripts** `drive`：L2 `DriveBegin`/`DriveEnd`、L4 step 结果（按规范接线）。
- **tx-kernel**：`trap.rs` / `thread_future.rs` / `trap_handoff.rs` 发瞬时记录与生命周期
  span；`init.rs` 在 boot 时调 `tx_observe::init::<P>(cpu_id)`；`init/exec.rs` 注册 dump
  关停钩子、设阈值、开关 emit、重置环。

主机侧 **`tools/tx-trace-daemon`**（独立 Rust 工程）：

- `replay --file <.txtrace> --out json|pftrace` —— 把环解码成 NDJSON 或 Perfetto protobuf；
- `bundle` —— 产出自包含目录（txtrace + ndjson + pftrace + 可选 names.json）；
- `live_guest_mem` —— 从 QEMU guest-RAM memory-backend 文件直接抽 tx-observe 环；
- 可选 `--filter level=<0..7>` 丢弃低于某 level 的记录；`--names` 解析 EventNameId。

---

## 8. 测试支持

`testing` feature（`crates/tx-observe/src/testing.rs`）提供 RAII 夹具，免去每个测试
手抄上百行平台样板：

```rust
#[test]
fn my_test() {
    let obs = TestPlatform::new().init();          // 默认 cpu_id=0, slot_count=16
    obs.emitter().instant(
        tx_observe::TxTraceLevel::Drive,
        tx_observe::EventNameId::from_raw(0x1),
        tx_observe::SpanId::NONE,
        tx_observe_types::TxPayloadTag::None,
        &[],
    );
    let records = obs.records();                    // 快照所有记录
    assert_eq!(records.len(), 1);
}
```

- `TestPlatform` 构建器：`.with_cpu_id(..)` / `.with_slot_count(..)` / `.init()`；
- `TestObservation` 守卫：`.emitter()` / `.records()` /
  `.records_of_kind(kind)` / `.header()`；`Drop` 时重置全局观测静态并释放测试锁；
- 内置 `SyntheticPlatform` 实现所有 tx-hal 平台 trait，用 thread-local 把堆上的环喂给
  `tx_observe::init`。

---

## 9. 实现状态总览

**已落地：**
per-hart `HartEmitter` 全套 API、SPSC 环生产者（Acquire/Release 内存序）、
`SpanId`/`EventNameId` + FNV-1a、L0 边界埋点（`traced_syscall!`）、
全部 payload 类型与编码函数（栈缓冲）、per-hart init 与环校验、dump 阈值触发、
串口 hex dump（QEMU 板提取用）、测试夹具。

**脚手架 / 暂缓：**
L1（Script）暂缓；L5/L6（Phase/Mutation）等 `RawTrace<P>` 订阅接线；
L3（Yield/resume）部分接线（规范 §15）。

**不在本 crate 范围：**
板级 observer 实现（如 rv64-qemu 的 ivshmem，在 `boards/tx-hal-*/src/observer.rs`）；
主机侧 daemon 逻辑（`tools/tx-trace-daemon/`）；Perfetto UI / 名字解析。

---

## 10. 锚点索引（便于现场点开）

- `crates/tx-observe/src/lib.rs:1` — crate 文档与不变量（OBS-1/3/4/5/9）
- `crates/tx-observe/src/lib.rs:80` — `fnv1a32` 编译期事件名哈希
- `crates/tx-observe/src/lib.rs:145` — `set_current_parent_span`（父 span 透传）
- `crates/tx-observe/src/lib.rs:360` — `HartEmitter::span_begin` 等 API
- `crates/tx-observe/src/lib.rs:789` — `current()` 取当前 hart emitter
- `crates/tx-observe/src/lib.rs:1374` — `init::<P>(hart)` per-hart 初始化
- `crates/tx-observe-types/src/record.rs:131` — `TxTraceKind`
- `crates/tx-observe-types/src/record.rs:178` — `TxTraceLevel`（8 级）
- `crates/tx-observe-types/src/record.rs:9` — `TxTraceRecord` 80 字节布局
- `crates/tx-observe-types/src/header.rs:84` — `TxTraceHartRing`
- `crates/tx-observe/src/encode.rs` — payload 编码
- `crates/tx-observe/src/testing.rs` — 测试夹具
- `crates/tx-shims/src/linux_syscall/mod.rs:517` — L0 syscall 边界汇聚点
- `tools/tx-trace-daemon/` — 主机侧解码 / Perfetto 导出
- 规范：`docs/Txv3/08_OBSERVATION_v1.md` / `08_OBSERVATION_SERIALIZATION_v0.md` /
  `08_OBSERVATION_HOST_v0.md`

> 版本基准：当前分支 `codex/test-remote-network` 工作树快照。行号可能随改动漂移，
> 引用前建议用 `grep -n` 复核函数名。
