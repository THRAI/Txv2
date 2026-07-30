# feature-network-refactor 合并保真度与网络栈结构审计

日期：2026-07-30

当前基线：`feature-network-refactor @ 6503312f`

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

下一轮优先恢复 FileOps 生产统一入口，再处理 TCP 动态 option/状态单一真相；
L2/L3 所有权拆分和 wait 收敛属于更大的结构重构，应按独立计划推进。curl
当前无功能阻塞；长期可复现性仍建议把 `latest-stable` 换成固定 Alpine
branch/源校验记录。
