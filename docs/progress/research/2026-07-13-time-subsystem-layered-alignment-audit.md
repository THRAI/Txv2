# 时间子系统分层对齐审计

日期：2026-07-13

范围：当前工作树相对于
`docs/design/01_substrate/HAL_v1.md`、
`docs/design/02_execution/TIME_WAKE_v1.md`、
`docs/stage2-documents/time_infra/TX_TIMER_SUBSYSTEM_DESIGN_CN.md`
以及 Txv3 调度/Step 约束的分层检查。网络栈定时器语义不在本次范围内。

## 结论

当前状态不是“时间子系统完全与设计一致”，而是：

- HAL 的硬件能力拆分、timekeeper 的基本模型、统一 deadline registry、
  typed RTC 路径和 owner-aware wake 的主要骨架已经落地。
- API/导出/旧路径 linter 在当前生产扫描范围内通过，但它主要检查命名、
  import 和导出边界，不能证明并发线性化、硬件 driver 唯一路径、时间 ABI
  语义或所有 RTC 队列调用形态。
- 当前应把状态标为“拓扑与 facade 部分闭合，核心语义和 driver ownership
  仍有偏差”，不能沿用“implementation-complete”作为整个时间栈的结论。

## 分层检查

| 层级 | 设计要求 | 当前状态 | 证据 |
|---|---|---|---|
| HAL 硬件能力 | 分离 monotonic counter、当前 hart deadline timer、可选 persistent clock；HAL 不拥有语义 timer 和调度 placement。 | 基本通过。RV64/LA64 QEMU 有 RTC，m1dock mock 走 `Unsupported`；真实板 RTC 仍是预留接口。 | `crates/tx-hal/src/lib.rs:1214-1241,1441-1463`; `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:628-655`; `docs/design/02_execution/TIME_WAKE_v1.md:876-905` |
| HAL 合同完整性 | 应明确 timer wake enable、跨 hart monotonic 一致性/校准和 driver ownership。 | 不完整。文档只有 set/cancel 等局部接口，没有明确 `enable_timer_wakeups` 时序、幂等性、跨 hart skew 上限或 deadline 失败策略。 | `docs/design/01_substrate/HAL_v1.md:2044-2067`; `docs/design/02_execution/TIME_WAKE_v1.md:940-955,1529-1541` |
| 核心时间语义 | realtime = monotonic + offset；set 操作更新 generation、VVAR、timer 通知，再可选 RTC writeback。 | 基本路径通过，但并发快照不闭合。offset 和 generation 是独立 atomic，读路径不采样 generation，多个 writer 也没有统一序列化。 | `crates/tx-services/src/time/wall_clock.rs:378-439`; `docs/design/02_execution/TIME_WAKE_v1.md:1040-1043,1116-1176` |
| 时间 ABI | 不支持的 clock id 必须显式 Unsupported/errno；CPU time、TAI、粗粒度/分辨率要与实现一致。 | 不通过。`PROCESS_CPUTIME_ID`、`THREAD_CPUTIME_ID`、`TAI` 被映射到 wall/monotonic；`ITIMER_VIRTUAL/PROF` 使用 wall monotonic；`clock_getres` 与 vDSO 分辨率不一致。 | `crates/tx-shims/src/linux_syscall/time.rs:280-290,325-329,510-595`; `crates/tx-services/src/time/clock.rs:11-17`; `crates/tx-vdso/src/vdso.S:214-220` |
| 数值边界 | timespec、periodic deadline、realtime-to-monotonic 转换和 VVAR shift/mult 必须有明确溢出/负值语义。 | 不通过。发现 saturating/普通加法混用、周期乘加未检查、deadline=0 与 disarmed 混淆、VVAR 左移边界未保护。 | `crates/tx-shims/src/linux_syscall/time.rs:229-246`; `crates/tx-subsystems/src/timerfd/mod.rs:197-217`; `crates/tx-services/src/time/wall_clock.rs:456-475,501-509` |
| VVAR/vDSO | seqlock 写入必须不可交错；初始化后应映射到用户态并由 exec/auxv 暴露。 | 不通过/证据不足。当前 publisher 没有 writer lock；初始 snapshot 与 hook 安装时序有风险；exec 仍将 `AT_SYSINFO_EHDR` 设为 `None`，用户态映射未完成。 | `crates/tx-subsystems/src/vdso/mod.rs:102-119`; `crates/tx-kernel/src/init.rs:562-564,613,925-929`; `crates/tx-scripts/src/process/exec/script.rs:1160-1170` |
| Timer registry | 所有非网络 deadline producer 使用一个 registrar；wheel 只保存 registration，语义对象保存 Linux 真值和 guard/token。 | 主体通过。reactor 持有一个 `ReactorDeadlineRegistry/TimerWheel`，周期由 timerfd/POSIX/ITIMER 语义对象 rearm；但 scripts 仍有受控 substrate bridge。 | `crates/tx-reactor/src/deadline_registry.rs:1-35`; `crates/tx-services/src/time/deadline.rs:124-202`; `crates/tx-subsystems/src/timerfd/mod.rs:192-217` |
| ActiveWait | deadline 应是 wait protocol attachment，统一保存 primary wait 与 deadline guard，并按语义角色取消。 | 部分通过。stale source/generation 过滤存在，但 `resolve_on_wait_source` 使用 `PrimarySleep` 而设计要求 `DeadlineAbort`，cleanup 顺序也相反，统一 `ActiveWait` 结构尚未落地。 | `crates/tx-scripts/src/drive.rs:518-540`; `crates/tx-reactor/src/wait.rs:126-192,917-941`; `docs/design/02_execution/TIME_WAKE_v1.md:1367-1416` |
| Reactor driver | 只有 `ReactorTimeDriver` 执行 due driving、求 next deadline 和硬件 deadline programming。 | 不通过。`tx-kernel/src/thread_future.rs` 在扫描 POSIX/ITIMER 后直接调用 `HalDeadlineTimer::set_deadline_ns`，绕过 reactor driver；另有 owner-aware route 但仍保留 captured waker 兼容路径。 | `crates/tx-kernel/src/thread_future.rs:529-563`; `crates/tx-services/src/time/platform.rs:34-65`; `crates/tx-reactor/src/runtime.rs:718-824`; `docs/design/02_execution/TIME_WAKE_v1.md:1512-1516,1790-1793` |
| SMP/steal 唤醒 | 读 owner、锁目标队列、锁内复核 owner，变化则重试；远端通过 IPI 唤醒。 | 有效路径和测试存在，但实现未完全达到锁内复核/重试协议；registry 依赖 entry removal 防重复 fire，也没有显式 single-driver/claim 或 deadline-change 通知协议。 | `crates/tx-reactor/src/runtime.rs:737-760`; `crates/tx-reactor/src/scheduler.rs:1026-1054`; `crates/tx-reactor/tests/reactor_smoke.rs:3081-3121`; `docs/design/Txv3/10_SCHED_SMP_v1.md:46-62` |
| RTC/devfs | board RTC 细节只在 HAL/board；通用层通过 `HalRtcDevice<P> -> RtcDeviceOps`，IRQ 先 ack 再发布事件。 | 基本通过。QEMU typed 路径、no-RTC unsupported 路径和 IRQ ack 已存在；真实板无硬件实证。linter 对 `queue.fire_with_post(...)` 形态覆盖不足。 | `crates/tx-kernel/src/irq.rs:162-195`; `crates/tx-services/src/time/platform.rs:67-95`; `crates/tx-fs/src/devfs/mod.rs:397-407`; `xtask/src/lint_invariants_time_wake.rs:171-174` |
| 边界控制 | facade trait、导出限制、旧接口退休和上层 raw import 应可机械阻止。 | 静态 gate 通过，但覆盖面不足以证明语义闭合。当前 linter 只对已建模模式报 0；它没有阻止 driver 直接 HAL deadline、RTC `fire_with_post` 变体，也没有检查 CPU/Tai/overflow/seqlock。 | `xtask/src/lint_invariants_time_layering.rs:26-268`; `xtask/src/lint_invariants_time_wake.rs:16-57,613-715` |

## 优先级

### P0：先修复正确性路径

1. 收回 `thread_future` 的直接 `HalDeadlineTimer` 编程，将 entry timer 的
   next-deadline 通知交给唯一 `ReactorTimeDriver`。
2. 给 realtime mutation 和 VVAR publication 建立单一 writer/sequence
   线性化协议，保证 reader 不会采到 offset、generation、VVAR 的混合快照。
3. 明确 clock ID 与 timer 类型的能力矩阵：CPU time/TAI 未实现时返回
   `EINVAL`/`Unsupported`，不要静默伪装成 monotonic/realtime；分别处理
   `ITIMER_VIRTUAL/PROF`。
4. 对所有输入、周期 rearm、realtime rebase 和 VVAR shift/mult 建立 checked
   arithmetic，并区分 disarmed sentinel 与合法的 epoch 0 deadline。

### P1：补齐架构协议

1. 在活动文档中建立 `TimekeeperIf` -> `ClockRead/RealtimeControl`、
   `TimerRegistrar` -> `DeadlineRegistrar`、calendar `RtcDeviceOps` -> ns-level
   `RtcDeviceOps` 的唯一 crosswalk，并明确哪一套是权威接口。
2. 明确 `PrimarySleep`/`DeadlineAbort` 等 `TimerGuardRole` 是语义 catalog
   还是实现细分；补齐 install 后的 driver reprogram/deadline-changed 协议。
3. 收敛 ActiveWait 的数据结构和 drop 顺序，完成 owner-aware steal 的锁内
   复核/重试协议，移除 captured waker 的 correctness 依赖。
4. 完成 vDSO 用户态映射与 `AT_SYSINFO_EHDR` 接入，或在设计中明确当前阶段
   只提供 kernel-side VVAR，不宣称完整 vDSO。

### P2：增强控制和证据

1. 扩展 linter 匹配 `fire_with_post`、service-adapter 直接 HAL deadline
   调用和新的接口别名；增加负向 fixture。
2. 为上述 P0 语义补 focused tests，再恢复 broad dirty-tree `--tests`。
3. 为 SiFive/2K2000 真实板 RTC 增加 board-level witness；QEMU 和 mock
   不能替代 hardware-in-loop。

## 当前验证

- `CARGO_TARGET_DIR=target/codex-time-topology cargo test -p xtask time_layering -- --nocapture`：24/24 通过。
- `CARGO_TARGET_DIR=target/codex-time-topology cargo xtask lint invariants time-layering`：0 findings。
- `CARGO_TARGET_DIR=target/codex-time-topology cargo xtask lint invariants time-wake-retired`：0 retired sites。
- `git diff --check`：通过。

这些结果证明现有静态边界 gate 没有回归，不证明上述 P0/P1 语义问题已经解决。
