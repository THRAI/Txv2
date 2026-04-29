# Network L3 Implementation Plan

## Phase A: 类型与错误模型

输入：执行契约 + active docs + 网络设计文档
输出：L3 所需基础类型、错误码、结果类型
验收：可编译；无外部模块硬依赖

## Phase B: StreamSocket 状态机

输入：Phase A
输出：`Init -> Connecting -> Connected / Listen -> Closed` 状态转移
验收：状态迁移完整；错误路径完整；返回 transition summary

## Phase C: DatagramSocket 状态机

输入：Phase A
输出：`Unbound -> Bound -> Closed` 状态转移
验收：状态迁移完整；地址绑定/发送接收语义明确

## Phase D: TCP 辅助结构

输入：Phase B
输出：`TcpBacklog` / `TcpConnection` / 连接队列与 publish summary
验收：不依赖 reactor 实现；支持 Blocked 重入语义表达

## Phase E: L3 对外契约

输入：Phase B/C/D
输出：`trait Socket`、`StepOutcome`、`WaitKey`、外部 port trait/stub
验收：只定义接口，不落地外部接线

## Phase F: 收敛与检查

输入：Phase A-E
输出：结构收敛、命名统一、最小编译验证
验收：按执行契约逐条通过

## 测试设计阶段（实现后）

实现完成后再输出测试设计：

1. 状态迁移矩阵
2. 错误码路径
3. Blocked -> Wake -> Re-observe
4. publish summary 与可观察行为一致性
