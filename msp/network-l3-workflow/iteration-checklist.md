# Network L3 Iteration Checklist

## A. 开始前

- [ ] 已阅读 `execution-contract.md`
- [ ] 已复核 `msp/tx-kernel-network-stack-design-v9.md` + 相关 active docs
- [ ] 已列出本轮关键术语及其来源章节（术语对齐表）
- [ ] 本轮目标已明确且属于 L3
- [ ] 本轮不涉及外部模块实现
- [ ] 本轮先 plan 后实现（若未确认 plan，不开始编码）

## B. 实施中

- [ ] 仅实现网络模块内部逻辑
- [ ] 所有外部交互保持 `trait` / `stub` / `TODO`
- [ ] 未引入 L1 trap/dispatch 真实接线
- [ ] 未引入 reactor 真实等待逻辑
- [ ] 未引入 DMA/IRQ 真实驱动逻辑
- [ ] 若发现越界需求，已暂停并记录 blocker
- [ ] 所有临时代码都已标注 `REFACTOR(net-l3)` + `RFX-*`

## C. 收尾前

- [ ] 改动范围符合边界（未越界）
- [ ] 实现内容与确认 plan 一致
- [ ] 未完成项已记录为 blocker/next step
- [ ] 本轮结果可被下一轮继续消费
- [ ] 新增 `RFX-*` 均已登记到 `refactor-register.md`
- [ ] 已执行检索：`rg -n "REFACTOR\\(net-l3\\)|RFX-" crates/tx-subsystems/src`
- [ ] 已执行术语一致性检查（代码术语与文档术语一致）

## D. 最终输出

- [ ] 回显本轮目标达成情况
- [ ] 回显边界遵守情况
- [ ] 回显 blocker 与下一步
- [ ] 回显本轮新增/变更的 `RFX-*` 清单
- [ ] 回显测试情况（范围、命令、结果、风险）
