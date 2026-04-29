# Network L3 Workflow

本目录只服务 `feature-network` 分支下的网络栈 L3 开发流程约束。

文件说明：

- `execution-contract.md`：执行契约（边界、禁做项、必须遵循的流程）
- `implementation-plan.md`：L3 分阶段实施计划（先确认再实现）
- `iteration-checklist.md`：每轮开发前/中/后检查单
- `refactor-register.md`：待重构点登记表（`RFX-*`）

执行原则：

1. 先看 `execution-contract.md`
2. 再按 `implementation-plan.md` 的 phase 顺序推进
3. 每轮结束按 `iteration-checklist.md` 自检
4. 所有临时实现必须写 `REFACTOR(net-l3)` 注释并登记到 `refactor-register.md`
