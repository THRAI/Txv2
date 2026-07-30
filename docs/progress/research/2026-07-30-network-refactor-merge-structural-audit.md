# feature-network-refactor 合并保真度与网络栈结构审计

日期：2026-07-30

当前基线：`feature-network-refactor @ 26e7d79c`

合并提交：`6d41a347`

合并前 feature 锚点：`premerge-feature-20260727 @ 90939012`

## 结论

07-27 的合并没有把网络重构主体整体覆盖掉。P0、P1、P4 的关键实现仍在，
P2 的外部 TCP/UDP 数据面和 P3-B/P3-C 的结构收敛也保留了。两个明确的
合并回退是：

1. P2 的 virtio-net IRQ top-half/bottom-half 链没有进入当前 HAL/kernel；
   当前外部网络依靠 10 ms RX 轮询下限兜底。
2. P3-A 的 Socket `FileOps` 实现文件仍在，但通用 VFS/syscall 调用点又退回
   socket 类型特判，trait 目前不是生产入口。

因此当前状态不是“重构没合进来”，而是“主要数据面已合入，两个统一入口
回退，若干原计划中的结构欠账仍在”。

## 合并保真度

| 阶段 | 当前判定 | 证据 |
|---|---|---|
| P0：真实时钟、常驻 context | 保留 | `net/clock.rs:17-28`；`net/protocol/tcp.rs:695-712`；`linux_syscall/mod.rs:994-995` |
| P1：TCP 单 smoltcp ring、删除 staging/直拷 | 保留 | `net/protocol/tcp.rs:177-211`；`step_tcp_loopback.rs:184` |
| P2：外部 connect/TX/RX、UDP、IPv6 demux | 数据面保留，IRQ 链回退 | `step_connect.rs:116-153`；`step_device_tx.rs:220`；`protocol/udp.rs:37`；`packet/smoltcp_demux.rs:23`；当前 `tx-kernel/src/irq.rs:162-176` |
| P3-A：FileOps/统一 fd 入口 | 实现保留，生产接线回退 | `net/file_ops.rs:30-76`；`vfs/structure.rs:1591-1594`；`linux_syscall/io.rs:1905-1906,2265-2280` |
| P3-B：SocketImpl、TCP 单锁、readiness 收敛 | 核心保留 | `net/structure/payload.rs:29-49`；`net/protocol/tcp.rs:38-60` |
| P3-C：关闭清理、资源上限 | 保留 | `step_socket_close.rs:51`；`netfilter.rs:158,877` |
| P4：checksum、分片 LRU/TTL、ether 文件拆分 | 保留 | `packet/smoltcp_demux.rs:81-146`；`protocol/ether/mod.rs:231-234,1116-1141`；`protocol/ether/{link,l3}.rs` |

机械对比显示，合并前 feature 的十个 P0～P4 核心锚点与当前 blob 完全一致；
当前 `tx-subsystems/net` 的变化主要来自合并后的修复，而不是整树回滚。

## 用户问题逐项判定

### TcpSocket 字段是否仍冗余

历史上的九个 `Option<RawXSocket>` 已经改成单一 `SocketImpl`，TCP 的
smoltcp socket、协议 sticky bits 和 cork buffer 也已经放进一个 `TcpInner`
锁，旧的五缓冲/三锁问题不再成立。

仍有三类冗余或多真相：

- 状态分散在外层 `TcpState`、smoltcp `Socket::state()`、
  `RawTcpProtocolState`、`SocketPayload.shutdown_rd/wr` 和 readiness sticky
  bits。`listen`/external connect 先更新外层状态，再忽略 engine 或索引错误，
  存在实际分叉窗口（`step_listen.rs:50-77`，
  `step_connect.rs:136-153`）。
- `recv_capacity/send_capacity` 与 sockopt 报告值保存用户配置，但真实 ring
  固定钳制为每方向 64 KiB（`protocol/tcp.rs:22-35,49-60,115`）。
- `SO_SNDBUF/SO_RCVBUF`、`SO_KEEPALIVE`、`IP_TTL`、`TCP_NODELAY`
  只修改 option；smoltcp setter 只在构造/reset 时调用。对已经创建的 engine
  调用这些 `setsockopt`，报告值会变化，运行行为不一定变化
  （`linux_syscall/socket.rs:2230-2275,2738-2748`，
  `protocol/tcp.rs:519-535`）。

### 网络层与链路层是否解耦

完成了文件级拆分，但没有完成所有权或接口级解耦。`link.rs` 和 `l3.rs`
都 `use super::*`，共同操作一个同时持有 ARP、NDP、IPv4 reassembly 和设备
引用的 `EtherIface`；L3 还直接调用 L2 发帧函数。因此应判定为“部分完成”：
可读性变好，边界尚未形成（`protocol/ether/mod.rs:140-153`，
`protocol/ether/link.rs:1`，`protocol/ether/l3.rs:1,135`）。

### Socket 是否实现文件系统接口

类型层面已经实现：`Cap<SocketIdentity>` 实现了 `FileOps`，`OpenFile` 也能
返回 `file_ops()`。生产调用层面尚未统一：`read/write` 仍显式转发到
`recvfrom/sendto`，poll/fcntl/close/ioctl/splice 仍有 socket backing 特判；
当前 `file_ops()` 没有生产调用点。判定为“接口存在，但 P3-A 接线被合并回退”。

### TCP 是否仍使用死时钟

主路径不是死时钟。delegate 每轮发布真实单调时间，常驻 smoltcp context
读取 `NET_NOW_NS`，socket syscall 分发也会刷新。

修复尚未完全闭合：

- bridge 初始值仍为 0；
- 若干公开兼容入口仍传 `Instant::ZERO`；
- syscall 刷新名单没有覆盖 socket 的 `read/write/readv/writev` 特判入口。

delegate 正常运行时这些入口通常只会看到略旧时间，不等同于重构前的全局
冻结；但它们是应收敛的时钟边界
（`net/clock.rs:17-28`，`delegate/runtime.rs:196-199`，
`step_device_tx.rs:79-92`，`step_process_network_events.rs:50-62`）。

## 仍存在的结构性问题

按优先级排序：

1. **恢复真实 NET_IRQ 链。** 当前 `devices.rs` 的“interrupt-driven”注释与
   `irq.rs` 实现矛盾；HAL 无 `NET_IRQ`，kernel 只安装 UART/RTC，外网依赖
   `init/net.rs:393-411` 的 10 ms poll floor。
2. **恢复 FileOps 统一生产入口。** 保留 netlink/ioctl/user-memory 等必要
   syscall 适配，但普通 read/write/poll/fcntl/close 应从 backing 特判回到
   trait/step 路径。
3. **消除 TCP 状态与 option 双真相。** 状态转换应以 engine 结果提交；
   动态 sockopt 要么作用到 engine，要么明确拒绝，不能只改报告值。
4. **收敛 wait/readiness 双轨。** carrier 同时注册 RawQueue 和 WaitSource，
   poll 又同时读取 live snapshot 与 sticky bits；这是防丢唤醒的过渡方案，
   不是最终单一真相。
5. **修复动态网络对象生命周期。** boot singleton 的 `Box::leak` 可接受；
   rtnetlink 创建的名称、bridge/veth/dummy/vlan 设备删除后仍永久泄漏。
6. **去掉环境相关硬编码。** 临时端口轮转池只有 64 个，per-netns 索引固定
   256/128/256；TCP cork 固定 1460，`TCP_MAXSEG` 固定按 MTU 1500 和 IPv4
   40-byte overhead 推导，没有读取实际出接口，也不区分 IPv6。
7. **补齐已暴露的功能缺口。** `IP_HDRINCL` 可设置但发送返回
   `EOPNOTSUPP`；IPv6 fragmentation 未实现；NDP RX 未校验 hop-limit 255；
   VLAN 只有 rtnetlink 元数据、无 tagging data path；`SO_ERROR` 仍返回合成
   结果。核心网络目录未发现显式 `todo!`/`unimplemented!`，欠账主要表现为
   `EOPNOTSUPP`、合成语义和被忽略的错误。

协议规定的 IPv4 header 长度、fragment mask、NDP hop-limit 255、默认 TTL
64 等不是问题；问题是本应来自设备/路由/运行状态的值也被写死。

## curl 镜像集成与实测

本地已有 RV64 Alpine ext4 只有 `libcurl`，没有 curl CLI；唯一现成 curl
二进制属于 LoongArch，不能复用。采用仓库现有 Alpine RV64 包闭包工具：

- 默认包集合加入 `curl`；
- 递归解析并提取 `libcurl`、musl loader、OpenSSL、CA bundle、zlib、
  brotli、zstd、c-ares、IDN、PSL、nghttp2 等；
- mutable `latest-stable` release/APK index 每次刷新，minirootfs/APK payload
  继续缓存，避免旧索引指向已下架版本后 404。

生成结果：

- rootfs：`target/rootfs/alpine-rv64-qemu`
- initramfs：`target/images/alpine-initramfs-rv64-qemu.cpio`
- curl：`8.21.0 (riscv64-alpine-linux-musl)`，支持 HTTP、HTTPS、HTTP/2、
  IPv6 和 OpenSSL。

QEMU 实测：

```text
curl 8.21.0 (riscv64-alpine-linux-musl) ... OpenSSL/3.5.7 ...
* Established connection to 10.0.2.2:18080
< HTTP/1.0 200 OK
TX_CURL_HOST_OK
CURL_RC:0
```

宿主只在 `127.0.0.1:18080` 暂时提供 `/tmp/tx-curl-www/index.html`，Guest
通过 QEMU SLIRP `10.0.2.2` 抓取；服务和 QEMU 在验收后均已终止。

## 验证与下一步

- `bash -n tools/images/fetch-alpine-rv64.sh`
- `git diff --check`
- 默认缓存经历旧索引 404 后，刷新为 Alpine 3.24.1 当前索引并完整重建
- 失效 `file://` mirror 下成功回退到完整本地 metadata/APK 缓存
- `cargo xtask image cpio --profile alpine --target rv64-qemu`
- QEMU Alpine + user networking：curl HTTP 200、marker 正确、rc=0
- `cargo -q xtask unit`：tx-kernel `114/114`、tx-scripts `166/166`；仍只有
  既有 3 个 tx-shims 断言失败和 tx-ext4 陈旧 `with_target` 测试 API
- `cargo xtask progress validate`：仍被既有
  `2026-07-24-network-time-integration.json` 的旧状态值 `completed` 阻断
- `cargo xtask lint docs`：仍为既有 23 个断链及 anchor/stale-vocabulary
  告警；本次新增 research/STATUS 未出现在失败列表

2026-07-30 续记：优先级 1 的真实 NET_IRQ 链已经恢复，RV64 QEMU Git 全链
9/9，IRQ 统计为 `claims=59/completions=59/wrong-hart=0/missing-device=0`；
设计、实现边界和调试证据见
`2026-07-30-net-irq-restoration-design.md`。10 ms floor 仍作为明确 watchdog，
不再是正常流量的主要推进路径。

## Socket 特判复核：不能机械“全删”

2026-07-30 对照合并前 feature 锚点 `90939012`、P3-A 四个落地提交
`1fea8ea7` / `372a352a` / `5a097f37` / `e3b426b4`、后续 netlink 修复
`52718fa2` 和当前生产路径后，原审计里“普通 read/write/poll/fcntl/close
都回 FileOps”的表述需要收窄：

| 路径 | 合并前拍板 | 当前判定 |
|---|---|---|
| 普通 INET/INET6 `read/write` | 通用 VFS `step_read/step_write -> FileOps` | 合并回退，应恢复 |
| netlink `write` | 显式走 `dispatch_netlink_send`，否则通用字节发送永久等待 | 必要特判，保留 |
| 无 mailbox 的 bootstrap socket read gate | 未就绪直接 `EAGAIN`，不能进入阻塞 drive | 必要特判，保留；只保留 gate，不保留全量 `recvfrom` 转发 |
| socket staging 上限 | 使用 64 KiB，而不是 TTY 的 4 KiB | 必要类型策略，保留 |
| `sendto/recvfrom/sendmsg/...` | 解析 sockaddr/msghdr/iovec/cmsg/user memory 后调用 net step | socket ABI 本身，保留 |
| `poll/select/epoll` | P3-S4 当时经 FileOps | main 后来新增统一 `query_fd_ready` facade；保留 facade，不应机械回滚，只需消除它与 FileOps 的重复真相 |
| `F_SETFL(O_NONBLOCK)` | 通用 flag mutation 后调 `FileOps::on_set_fl_nonblock` | 当前又直接访问 socket readiness，属回退，应恢复 hook |
| `close/close_range/process exit` | 保留 retain-count/两相时序 bolt-on，只把种类语义交给 `FileOps::on_last_close` | 当前又直接匹配 Socket 并调 net close，属回退；时序壳保留、分派恢复 hook |
| socket ioctl | 因 Linux request 解码、`SyscallCtx` 和 usercopy 保留 shim 特判 | 必要特判，保留 |
| splice 拒 socket | 当前未实现 splice-socket，返回 `EINVAL` 是合法能力边界 | 保留 |
| `socketpair(AF_UNIX)` | `OpenFileBacking::SocketPair` 是双 PipePayload，不是 SocketIdentity | 独立形态，保留 |

当前 `net/file_ops.rs` 与合并前 blob 完全一致，真正丢的是接线：

- `OpenFile::step_read/step_write` 的 `StructPayload::Socket` 臂当前返回
  `EINVAL`；
- `sys_read/sys_write` 对所有 socket 整体转发到 `recvfrom/sendto`；
- `F_SETFL`、last-close 和 process-exit 又直接匹配 Socket；
- 全仓生产代码没有 `file.file_ops()` 调用，只有访问器定义；
- P3-S2 判决测试
  `open_file_read_write_delegate_to_socket_file_ops` 当前稳定失败于
  `Err(EINVAL) != Done(4)`。

恢复方案应分成可回滚的小步：

1. 先恢复 VFS 的 Socket read/write 委派，使既有判决测试转绿；此时 syscall
   转发仍挡在前面，用户态行为不变。
2. 再摘掉普通 socket 的 `read/write -> sendto/recvfrom` 整体转发，同时显式
   保留 netlink write、bootstrap gate 和 64 KiB staging；补齐 EINTR/itimer、
   SIGPIPE、loopback fairness、大 UDP datagram、readv/writev 与 netlink
   不挂死的回归见证。
3. 保留当前 `query_fd_ready` 作为 poll/select/epoll 的统一 fd facade；单独
   决定让它经 fd-neutral ops 查询，还是从 `FileOps` 删除已经被 facade
   取代的 `PollMask` 方法。当前 `device::FileOps` 直接暴露
   `crate::net::PollMask`，与 P3 文档“trait 对 net 零依赖”的目标矛盾，不能
   原样照搬旧接线。
4. 恢复 F_SETFL/last-close hooks，但保留 close 的 retain-count 两相时序；
   ioctl、splice 拒绝、socketpair 和所有 socket 专属 syscall 不动。

因此下一轮不是“消灭 socket 特判”，而是恢复**普通文件语义的统一入口**，
把 Linux ABI、启动期和未实现能力边界的特判集中保留。完成后再处理 TCP
动态 option/状态单一真相；L2/L3 所有权拆分和 wait 收敛属于更大的结构
重构。curl 当前无功能阻塞；长期可复现性仍建议把 `latest-stable` 换成固定
Alpine branch/源校验记录。
