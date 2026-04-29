# Network L3 Execution Contract

## 1. 目标与边界冻结

1. 只实现 L3（协议实现层）代码。
2. 只改网络模块目录，不实现任何内核外部模块逻辑。
3. 与外部模块交互全部保留为 `trait` / `stub` / `TODO` 接口，不落地接线。
4. 不做 L1 trap/dispatch。
5. 不做 reactor 真等待。
6. 不做真实驱动 DMA/IRQ。

## 2. 规范基线（实现依据）

1. 只用 active 文档 + `msp/tx-kernel-network-stack-design-v9.md` 作为规范源。
2. 每个 phase 开始前必须先复核以下文档，再写计划或代码：
   - `msp/tx-kernel-network-stack-design-v9.md`
   - `docs/design/INDEX.md`
   - `docs/design/00_meta-framework/CONCEPTS_v4.md`
   - `docs/design/00_meta-framework/INVARIANTS_v4.md`
   - `docs/design/00_meta-framework/object_model_v2.md`
   - `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`
   - 与当前 phase 相关的 active 子系统文档
3. 使用技能约束：
   - `tx-design-reference`
   - `tx-implementation-readiness`
   - `tx-agentic-development`
   - `tx-progress-memory`
4. 五阶段语义只在接口契约体现；L3 实现聚焦状态机与 transition summary。
5. 术语对齐是硬规则：计划与代码中的专业术语必须与架构文档和 active 文档一致，不得私自改名或引入未定义同义词。
6. 若需要新增术语，必须先在计划中给出“来源文档锚点 + 映射关系”，经用户确认后才能使用。

## 3. 实施流程（强制）

1. 先 plan，待用户确认后再编码。
2. 编码必须严格按确认后的 plan 实施；中途变更需先确认。
3. 实现完成后再做测试设计，不提前跨阶段。
4. 计划必须覆盖本轮要实现的全部代码项（文件、类型、函数、关键字段）；禁止“实现时再临时补内容”。
5. 计划与代码均禁止伪代码；必须使用可编码结构描述：`struct/enum/trait/type/fn signature` 与明确模块路径。

## 4. 变更控制

1. 默认只改 `msp/` 与网络模块相关代码路径。
2. 若需改非网络模块，必须先得到用户明确确认。
3. 若发现外部模块缺口，记录为 blocker，不在本轮越界实现。

## 5. 每轮固定回显

每轮都要回显三点：

1. 本轮目标
2. 本轮是否完全遵守边界
3. 本轮未解决 blocker

## 6. 待重构标记（强制）

1. 所有临时实现必须在代码旁标注 `REFACTOR(net-l3)` 注释。
2. 所有标记必须带唯一 ID：`RFX-001`、`RFX-002`、`RFX-...`。
3. 典型场景包括：`stub`、`TODO`、模拟等待、临时错误分支、未来 reactor 接线点。
4. 推荐注释模板：

```rust
// REFACTOR(net-l3): [RFX-001] 接入 reactor 后替换 StubWaitRuntime。
// Trigger: reactor wait token ready
// Keep-until: L1/L2 接线完成
```

5. 每新增一个 `RFX-*`，必须同步登记到 `refactor-register.md`。
6. 每轮收尾必须执行检索命令，确保无遗漏：

```bash
rg -n "REFACTOR\\(net-l3\\)|RFX-" crates/tx-subsystems/src
```

## 7. 术语对齐检查（强制）

1. 每轮开始前，先列出本轮将使用的关键术语，并标注来源章节（网络架构文档 + active 文档）。
2. 每轮收尾前，执行术语一致性检查，确认代码命名与文档术语一一对应。
3. 若发现术语不一致，优先回改代码/计划命名，不做“口头解释保留”。
