# 网络子系统阅读期基线

日期：2026-08-03

## 目的与边界

本记录启动网络子系统的“先通读、先记账、后修复”阶段。当前阶段只做：

- 固定当前分支、HEAD、canonical 设计文档和已有验证证据；
- 建立 syscall → socket object → protocol → device/IRQ → wait/readiness →
  fd close 的源码链路；
- 把发现分为“当前代码已证实”“待核实风险”“历史记录漂移”，不把设计设想
  当成实现事实；
- 不修改网络实现，不进行结构重构，不覆盖另一会话正在进行的上板移植工作。

当前工作树是 `feature-network-refactor-recovery`，HEAD 为
`7eb13c9ebc5b4c0b12cce62dc49023bb850534ed`（merge:
`integrate main and preserve dual-arch network recovery`）。工作树已有用户的
`docs/progress/STATUS.md` 未提交修改；`.codegraph/` 是未跟踪的 CodeGraph
索引目录。本轮保留两者，不把它们当作本轮改动。

## 当前源码主链路

```text
socket syscall
  -> SocketIdentity / OpenFile
  -> SocketPayload / SocketImpl
  -> bind/listen/connect/send/recv/poll/close steps
  -> network delegate
  -> EtherIface / smoltcp / NetDevice
  -> IRQ deferred claim + device RX/TX
  -> readiness RawQueue + WaitSource / fd readiness
  -> last-close and protocol cleanup
```

第一轮 CodeGraph 与源码核对得到的当前入口如下：

| 层次 | 当前事实 | 证据 |
|---|---|---|
| syscall | `NR_SOCKET` 进入真实 socket shim；`sys_socket` 校验参数、取得 net namespace、创建 OpenFile 并写入 fd table | `crates/tx-shims/src/linux_syscall/mod.rs:1104`; `crates/tx-shims/src/linux_syscall/socket.rs:136` |
| object | socket backing 是 `StructPayload::Socket`；identity 负责稳定身份和 readiness carrier，payload 可替换 | `crates/tx-subsystems/src/net/execution/step_socket_open_file.rs:79`; `crates/tx-subsystems/src/net/structure/identity.rs:22,121` |
| protocol | `SocketPayload` 以 `SocketImpl`/协议状态选择 TCP、UDP 及其他 socket 引擎；TCP/UDP 创建对应 raw smoltcp socket | `crates/tx-subsystems/src/net/structure/payload.rs:129,206,243` |
| connection | bind/listen/accept/connect 分别维护 table、协议状态、backlog 和 connect attempt；TCP external connect 通过 smoltcp 发 SYN | `crates/tx-subsystems/src/net/execution/step_bind.rs:12`; `step_listen.rs:11`; `step_accept.rs:18`; `step_connect.rs:19-128` |
| byte I/O | send 在空间不足时清除 `SPACE` 并 yield；recv 在无数据且 peer 未关闭时 yield | `crates/tx-subsystems/src/net/execution/step_send.rs:23-80`; `step_recv.rs:13-68` |
| delegate | 每次 delegate poll 先发布单调时钟，然后处理 RX demux、loopback、device TX、ARP/NDP 和 namespace runtime；tick 处理 backlog/deadline | `crates/tx-subsystems/src/net/delegate/runtime.rs:192-315` |
| packet/device | `EtherIface` 持有链路/IP 邻居和 IPv4 fragment 状态；packet TX sink 负责路由、邻居解析和发送；virtio poll 负责设备完成事件 | `crates/tx-subsystems/src/net/protocol/ether/mod.rs:140-153,833-839`; `crates/tx-drivers/src/virtio/net.rs:321` |
| IRQ | 网络 IRQ 顶半部只发布当前 hart 的 deferred claim；实际 ACK、设备 poll/kick 和 controller completion 在任务上下文完成 | `crates/tx-kernel/src/irq.rs:314,346-359` |
| readiness | socket 同时保留 `RawQueue` 与 substrate `WaitSource` mirror；poll 用 live `io_snapshot` 加 sticky bits 生成 `PollMask` | `crates/tx-subsystems/src/net/structure/readiness.rs:40-129`; `crates/tx-subsystems/src/net/execution/step_poll.rs:13-175` |
| fd/VFS | `query_fd_ready` 将 socket poll mask 转成 fd readiness 并返回 wait source；普通 VFS read/write 通过 `FileOps` 进入 socket byte steps | `crates/tx-subsystems/src/vfs/fd_ready.rs:342-360`; `crates/tx-subsystems/src/vfs/execution.rs:402-406`; `crates/tx-subsystems/src/net/file_ops.rs:27-64` |
| close | `close` 先移除 fd；最后一个 OpenFile 引用释放后调用 `FileOps::on_last_close`。当前 connected TCP 分支保留 payload/table-owned socket，要求 delegate 继续推进关闭；其他分支撤销绑定并释放 payload | `crates/tx-subsystems/src/process/execution.rs:1260,2377`; `crates/tx-subsystems/src/net/execution/step_socket_close.rs:63-74,176-208` |

## 第一批需要继续核对的项目

这些是阅读任务，不是已经判定的 bug：

1. TCP 状态的单一真相：外层 `TcpState`、raw smoltcp state、connect generation、
   shutdown 标记、readiness bits 之间的提交顺序和失败回滚。
2. TCP close 的完整生命周期：当前源码已经有 deferred close 分支；仍需把
   `step_tcp_close`、delegate tick、FIN/TIME-WAIT/reap、OFD 的显式和隐式释放
   一起读完，才能判断是否完整符合 Linux 语义。
3. readiness 双载体：`RawQueue` 与 `WaitSource` 是否在所有 fire/clear/close/
   connect-error 路径保持同一语义，尤其是 poll/epoll 与阻塞 read 的交错。
4. L2/L3 边界：文件已拆成 `link.rs`/`l3.rs`，需要继续确认 `EtherIface` 的所有权、
   route/neighbor/reassembly 的修改入口是否仍然交叉。
5. 动态网络对象生命周期：rtnetlink 创建/删除 bridge、veth、dummy、vlan 和
   interface registration 后的引用释放与 namespace 回收。
6. ABI 分支覆盖：普通 INET TCP/UDP 与 netlink、AF_PACKET、AF_UNIX、SCTP、
   RDS 等分支的错误码、阻塞语义和用户内存拷贝边界。

## 历史记录的使用规则

当前 progress 中的双架构 Git/HTTP/HTTPS/DNS 和 netperf/iperf 记录可作为已完成
验证的线索，但不能替代当前 HEAD 的复跑；完整 LTP/OSComp 网络矩阵也不能因此
宣称关闭。Phase 1～8、retained-close 设计、SO_LINGER、readiness 收敛和动态对象
生命周期设计均是历史/延期材料，只有在当前源码和当前 witness 再次核对后才能
进入实现计划。

特别注意：`docs/progress/research/2026-07-30-tcp-retained-close-design.md`
记录的旧 close 形状与当前 `step_socket_close.rs:63-74` 已出现差异。这不是自动
判定旧记录错误，而是一个必须记录的 commit 漂移信号：后续说明应同时写明 HEAD
和源码行号。

## CodeGraph 使用约定

当前索引已是 up-to-date（1,766 files、48,916 nodes、204,222 edges）。推荐阅读
时使用以下顺序：

```sh
codegraph status
codegraph explore --max-files 18 "trace <symbol or behavior>"
codegraph node <symbol>
codegraph callers <symbol>
codegraph callees <symbol>
codegraph impact <symbol>
```

`explore` 适合先找相关符号和调用路径，`node` 适合读取一个符号或指定文件的
行号片段，`callers/callees` 适合补齐跨模块边，`impact` 适合未来修改前做影响面
检查。CodeGraph 的动态 trait/宏边可能不完整；每个最终结论仍需用当前源码和测试
文件核对，并记录精确行号。

## 下一步

按以下顺序继续通读：

1. `step_tcp_close`、`step_tcp_cleanup`、delegate tick 与 close tests，闭合 TCP
   连接生命周期；
2. `step_process_network_events`、`packet/smoltcp_demux.rs`、TCP/UDP ingress，
   闭合 RX 路径；
3. `step_device_tx`、`EtherIface::dispatch_ip_at`、virtio net device，闭合 TX
   和邻居解析路径；
4. `socket.rs` 的 syscall ABI helper 与 `vfs/fd_ready.rs`/epoll，闭合用户态入口；
5. 依据实际代码差异建立独立问题清单，之后再为每个修复选择最小 witness。

本阶段的完成条件不是“网络目录读过一遍”，而是每条关键链路都能从用户入口
追到状态变更、唤醒/等待、设备交互、错误返回和释放路径，并且每个问题都有当前
源码证据或明确的待核实状态。
