# TCP retained close / SO_LINGER 调研与实施边界

状态：2026-07-30 调研完成，按用户要求暂停实现。前一批 generation-safe TCP
生命周期修复已提交为 `64b48af6`，本轮没有修改代码。

## 结论

当前 last fd close 不是 TCP graceful close。它最多尝试 8 轮本机 loopback
数据搬运，随后撤销 connection/bind index、调用 `RawTcpSocket::abort()`，
并从 `SocketIdentity` 取走整个 `SocketPayload`。smoltcp 已有
FIN_WAIT1/FIN_WAIT2/CLOSING/TIME_WAIT 和可推进网络时钟，但承载这些状态的
transport 已被销毁，FIN/ACK/RTO 从此没有 demux、poll 或 deadline 入口。

因此不能用“close 时多调用一次 `raw.close()`”修复。必须让 TCP endpoint
脱离 fd/OpenFile 生命周期继续由网络层持有，直到 FIN/RST 已发送、终态到达
或 deadline 触发回收。

`SO_LINGER` 当前只有 setsockopt/getsockopt 存取，没有任何 close/shutdown
消费点。`shutdown(SHUT_WR)` 是现有唯一真实 FIN 路径：它调用
`RawTcpSocket::close()`、保留 payload/index 并 kick delegate；紧接着
`close(fd)` 仍会把这条尚未完成的 FIN 生命周期销毁。

## 关键证据

| 结论 | 证据 |
|---|---|
| last-close 撤索引、abort 并 take payload | `crates/tx-subsystems/src/net/execution/step_socket_close.rs:40-72,174-183` |
| `peer_detached` 是拓扑拆除，不是 wire FIN | `crates/tx-subsystems/src/net/execution/step_socket_close.rs:195-247` |
| shutdown write half 会启动真实 FIN 并 kick delegate | `crates/tx-subsystems/src/net/execution/step_shutdown.rs:28-108` |
| linger 只是 option 字段 | `crates/tx-subsystems/src/net/structure/types.rs:575-586` |
| FileOps last-close hook 无 context、deadline 或 completion | `crates/tx-subsystems/src/device.rs:262-278`; `crates/tx-subsystems/src/net/file_ops.rs:62-64` |
| close 固定为不可 Yield 的 OneShotStepOp | `crates/tx-shims/src/linux_syscall/fs_basic.rs:1525-1565`; `crates/tx-subsystems/src/process/execution.rs:2196-2220` |
| delegate deadline 当前主要汇总 listener backlog | `crates/tx-subsystems/src/net/delegate/runtime.rs:210-245,286-320`; `crates/tx-subsystems/src/net/execution/step_process_network_events.rs:209-233` |
| namespace 内强持有 socket 会形成 namespace→socket→namespace 环 | `crates/tx-subsystems/src/net/namespace.rs:55-90,370-415`; `crates/tx-subsystems/src/net/structure/payload.rs:206-217` |
| dup3 replacement 与 exec CLOEXEC 只 drop 旧 file，绕过 last-close | `crates/tx-subsystems/src/process/execution.rs:2245-2295`; `crates/tx-subsystems/src/process/exec_prep.rs:245-280` |

## 推荐实施顺序

### P0：统一 OpenFile 最后释放入口

先抽出单一 OFD release helper，使显式 close、close_range、process exit、
dup3 replacement 和 exec CLOEXEC 都执行同一套：

1. 移除 fd；
2. 判断是否为最后一个 OpenFile 引用；
3. 以明确 `CloseOrigin` 调用 kind-specific begin-close；
4. 显式 close 可返回 completion，exit/exec/dup3 只启动后台 close。

这一步先补判决测试，不改变 TCP transport 行为。否则后面即使有 closing
registry，隐式 close 仍会漏 FIN、漏 RST或永久占用 tuple。

### P1：引入有界 `TcpClosingRegistry`

最小风险实现先保留现有 `SocketIdentity`/connection-index demux，不立刻重写
整个 TCP 数据模型。last-close 把一个强 `Cap<SocketIdentity>` 转交给网络级
closing registry，保留 payload、四元组、generation 和 index：

- 默认 linger disabled：调用 transport `close()`，fd 立即返回，后台送完
  数据/FIN 并等待终态；
- linger enabled + timeout 0：不做 8 轮应用数据 flush，调用 `abort()`，
  kick TX，至少保留到 RST 获得一次发送机会后回收；
- linger enabled + timeout > 0：同 graceful close，但记录单调 deadline 和
  独立 close completion。

registry 第一阶段应是网络运行时的显式全局根，而不是直接放进
`NetNamespacePayload`：后者强持有 socket，而 `SocketPayload` 又强持有
namespace，会形成引用环。全局 entry 通过 socket 间接 pin namespace，reaper
删除 entry 后 namespace 才可按现有 retain-count 规则回收。registry 必须有
容量上限、round-robin cursor、统计和超时兜底，禁止变成永久 orphan 仓库。

### P2：接入 delegate POLL/TICK 与原子 reaper

POLL/TICK 公平扫描 closing entry，并把每个 raw TCP 的 `poll_at()`/linger
deadline 汇总进 `NetDelegateRuntimeOutcome::next_deadline`。现有 connection
index 在 closing 期间继续 demux FIN/ACK/RST；达到 smoltcp terminal state 或
deadline 后，使用 generation/owner-safe reservation 事务撤销本端 forward
index 与 bind，随后 `take_payload()` 并释放 registry owner。

真实 close 不应像当前 `AF_UNSPEC` 一样删除 peer 的 reverse index：peer 是独立
endpoint，必须继续收数据、观察 FIN，并按自己的生命周期关闭。TIME_WAIT
期间本端 tuple/index 继续保留，避免过早复用。

### P3：positive linger 改为可等待 close

显式 `close(2)` 需要从无条件 OneShot 拆成 close-specific StepOp：

- fd 仍在开始时移除；
- positive linger 等待独立 `TcpCloseCompletion` 或 deadline；
- process exit、exec CLOEXEC、dup3 replacement 不阻塞，只走后台模式；
- completion 不能复用 `SendWireSet::BROKEN`，因为 shutdown 在 FIN 刚开始时
  就会发布 send-broken，它不代表 FIN/RST 已完成。

精确的 signal/errno 与 `O_NONBLOCK + SO_LINGER` 组合需先做 Linux witness，
不能凭印象硬编码。

### P4：后续再抽取 net-owned `TcpFlow`

P1 暂时保留整个 SocketIdentity，因而也延寿 fd-facing readiness/wait carrier。
它是有界、可验收的过渡结构。确认 FIN/RST/reaper 语义后，再把
`RawTcpSocket + tuple + generation + close policy` 抽成 net-owned
`TcpFlow`，SocketIdentity 只保留可脱离的 fd attachment。不要在第一步同时
改 table value、ingress demux、poll publish、procfs 和 close ABI。

## 必须先有的测试

- default close：loopback 与 device-TX 各一条真实 FIN，queued data 先到达；
- `SO_LINGER(on, 0)`：真实 RST，未发送应用数据不再被 8-pass flush；
- positive linger：park、completion wake、deadline 三条；
- `shutdown(SHUT_WR); close()` 不取消已经开始的 FIN；
- dup/fork 最后引用、dup3 replacement、close_range、CLOEXEC、process exit；
- closing entry 的 tuple 不提前复用，终态/超时后可重绑；
- namespace 销毁后 registry 无残留，容量满时行为有明确 errno/降级策略。

## 暂停点

下一步从 P0 的“统一 OFD release 判决测试”开始，不先改 `step_socket_close`。
实现前还需两个精确 witness：smoltcp 各 close state 的 terminal/reap 判据，
以及 Linux 对 positive linger、`O_NONBLOCK`、信号中断的返回语义。
