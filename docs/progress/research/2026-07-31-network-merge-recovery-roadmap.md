# feature-network-refactor 合并恢复总账与后续路线图

日期：2026-07-31

状态：恢复审计完成；当前只执行 merge 前功能验收，Phase 1～8 已搁置。

基线：

- 合并前 feature 锚点：`premerge-feature-20260727 @ 90939012`
- 发生回退的合并：`6d41a347`
- 本文审计的当前 HEAD：`feature-network-refactor @ ad115d68`
- 可执行进度计划：
  `docs/progress/plans/2026-07-31-network-merge-recovery.json`
- 当前唯一验收矩阵：
  `docs/progress/research/2026-07-31-premerge-network-recovery-test-matrix.md`

## 2026-07-31 范围修正

用户确认当前目标只是在 current HEAD 上恢复并证明 merge 前已有功能，不继续
完成网络栈长期重构。因此本文原列 Phase 1～8 全部搁置：

- 它们不是已知 merge 回退；
- 它们多数在 `90939012` 时本来就没有完整实现；
- 它们不再是本轮恢复完成条件；
- Phase 0 通过后立即关闭恢复，不自动进入 Phase 1。

当前准确命令、历史分数和 verdict-set 以
`2026-07-31-premerge-network-recovery-test-matrix.md` 为准。

## 一句话结论

如果“恢复”是指**补回 07-27 merge 丢失的网络代码与入口**，当前没有发现仍未
恢复的已知代码缺口：两个明确回退——RV64 `NET_IRQ` 链和 Socket/FileOps
生产接线——都已经在 07-30 恢复，当前 TCP connect、时钟、generation、
`SO_ERROR` 和部分动态 sockopt 还比 merge 前更完整。

如果“恢复”是指**达到 feature-network-refactor 最初想要的完整网络架构**，
则尚未完成。最重要的剩余工作是 TCP last-close/FIN_WAIT/TIME_WAIT/
`SO_LINGER`；之后还有 readiness 双载体、动态网络对象回收、配置硬编码、
L2/L3 所有权和若干协议功能缺口。这些多数在 `90939012` 时就已经存在，
不能再称为“merge 回退”。

当前最合理的下一步不是立刻继续重构，而是先补一轮 post-merge 验收证据；
验收不退化后，再从“统一 OFD 最后释放入口”的判决测试开始 retained TCP
生命周期工作。

## “恢复完成”必须拆成三个层次

| 层次 | 含义 | 当前状态 | 完成判据 |
|---|---|---|---|
| R1：代码恢复 | merge 丢失的实现和生产入口重新存在 | **完成，无已知缺口** | premerge/current 机械对比 + 两项明确回退关闭 |
| R2：行为恢复 | merge 前的真实用户态能力在当前 HEAD 重新通过 | **部分完成** | RV64/LA64、Git/curl、netperf/iperf、LTP/OSComp 证据齐全 |
| R3：重构目标完成 | merge 前尚未完成的结构与 Linux 语义也全部收敛 | **未完成** | retained close、readiness、对象生命周期、L2/L3、协议缺口逐项验收 |

后续汇报必须明确说自己关闭的是 R1、R2 还是 R3。只跑通一次 curl 不能宣称
R2 全部完成；补回 FileOps 入口也不能宣称 R3 完成。

## 07-30 到底做了什么

### 1. 先修复作为网络见证前提的 Git/文件系统问题

| 提交 | 做了什么 | 结果 | 未覆盖边界 |
|---|---|---|---|
| `6503312f` | 修复 `index-pack` 后 fsync/完成唤醒、fork/CoW 边界和 DNS UDP 包被 loopback 提前消费 | Git/DNS/HTTP/HTTPS 从 3/8 到 8/8；fsync 22/22、fork 6/6、外部 UDP 1/1 | 不是纯网络提交；部分 planner/SMP/断电持久化问题不在本轮 |
| `add3faec` | 把 Alpine RV64 curl 及依赖加入镜像闭包，修复 mutable APK metadata 永久缓存 | QEMU guest curl HTTP 200，`TX_CURL_HOST_OK` | Alpine 源仍非 bit-for-bit 固定 |

这一步很重要，但它不等于网络栈结构恢复。它建立了后续网络修改可以依赖的
真实 guest Git/curl 见证。

### 2. 审计 merge 保真度，而不是直接把旧分支整棵搬回来

`add3faec` 同时记录了
`docs/progress/research/2026-07-30-network-refactor-merge-structural-audit.md`。
机械对比确认 P0/P1/P4、外部 TCP/UDP 数据面、单 `SocketImpl` 和 TCP 单锁
主体仍在。真正的已知 merge 回退只有：

1. RV64 virtio-net `NET_IRQ` top-half/bottom-half 链丢失；
2. Socket `FileOps` 文件仍在，但 VFS/syscall/F_SETFL/last-close 的生产接线
   丢失。

因此没有采用“回滚 main”或“把 `90939012` 全量 cherry-pick 回来”的办法。
旧 IRQ 顺序本身也有 PLIC completion 风险，不能机械恢复。

### 3. 恢复真实 NET_IRQ，但保留安全 watchdog

| 提交 | 做了什么 | 关键语义 | 验证 |
|---|---|---|---|
| `4cffab58` | 恢复 RV64 virtio-net deferred IRQ 链 | top half 只发布 claim；owner hart bottom half 先 ACK/poll/kick，再 PLIC complete；reactor 用有限 poll budget 给 bottom half 运行机会 | RV64 Git/DNS/HTTP/HTTPS 9/9；`claims=59/completions=59/wrong-hart=0/missing-device=0`；双架构 build |

这比 merge 前的 `mask -> pending -> early complete -> ACK -> unmask` 更安全。
当前仍保留 10 ms RX poll floor 作为 QEMU 冷启动/漏中断 watchdog；LA64 因
virtio-pci IRQ 路由未证明，仍使用 `NET_IRQ = 0` 的 poll-backed 路径。这两个
边界是显式设计，不是漏做。

### 4. 选择性恢复 Socket/FileOps，而不是删除所有 socket 特判

| 提交 | 作用 |
|---|---|
| `3f6f8dd7` | 先界定必须保留的 netlink、bootstrap、64 KiB staging、ioctl、socket ABI、splice 拒绝和 pipe-backed socketpair 特判 |
| `1ae1a789` | 恢复 VFS `OpenFile::step_read/step_write -> FileOps` Socket 委派 |
| `8b7bd4ac` | 普通 INET/INET6 `read/write` 回到通用 fd/FileOps；补齐 netlink payload 字节消费 |
| `b2d4e1a6` | 恢复 `F_SETFL(O_NONBLOCK)`、last-close、process-exit 的 FileOps hooks |
| `438037f3` | 保留并确立 `query_fd_ready` 为 poll/select/epoll 唯一 fd readiness facade，删除 `FileOps -> net::PollMask` 反向依赖 |
| `653135eb` | 记录四步恢复结果 |

该批次通过 socket fdtable 97/97、RV64 build、QEMU 9/9，IRQ
`61/61`。它恢复的是**普通文件语义统一入口**，不是把所有 socket syscall
都强塞进 FileOps。

### 5. 收紧 TCP connect、动态 option 和数据面边界

| 提交 | 做了什么 | 当前边界 |
|---|---|---|
| `284bc6d3` | 未连接 stream 普通字节 I/O 返回 Linux 形状的 `EPIPE`/`ENOTCONN`，避免 bootstrap read 永久 park | 只处理未连接 I/O 判决 |
| `f4ca10ad` | `TCP_NODELAY`、`SO_KEEPALIVE` 真正更新 live smoltcp engine，getsockopt 读 live engine | buffer/TTL/MSS 等仍未全部动态化 |
| `92a846b1` | 跨 netns TCP 强制经 veth/bridge/device 数据面；分段使用实际 egress MTU | L2/L3 所有权仍未拆分 |
| `64b48af6` | 统一 TCP control 临界区、connect generation/tuple、独立 `CONNECT_DONE`、one-shot `SO_ERROR`、真实时钟/127 s deadline、索引事务、loopback 公平性 | last fd close 仍会 abort/take payload；不能保留 FIN_WAIT/TIME_WAIT |

`64b48af6` 的聚焦验证为 TCP lifecycle 27/27、external connect 14/14、
veth 4/4、clock 2/2、network tick 4/4、socket fdtable 102/102、
tx-substrate 41/41。

### 6. 最后只做了 retained-close 设计，没有继续改代码

`ad115d68` 只新增
`docs/progress/research/2026-07-30-tcp-retained-close-design.md`。
它确认当前 last-close：

1. 先尝试搬运少量本机 loopback queued bytes；
2. 撤 connection/bind index；
3. 调用 transport `abort()`；
4. 从 `SocketIdentity` `take_payload()`。

所以当前 `peer_detached` 只是本机拓扑拆除，不是 wire FIN。
`shutdown(SHUT_WR)` 才会调用 transport `close()` 发起真实 FIN，但随后
`close(fd)` 仍会销毁这条生命周期。`SO_LINGER` 目前只有 ABI 存取。

## 当前与 merge 前的能力对账

| 原重构阶段 | 当前判定 | 是否仍是 merge 恢复缺口 |
|---|---|---|
| P0：真实时钟、常驻 smoltcp context | 保留，connect/RTO/deadline 进一步收敛 | 否 |
| P1：单 smoltcp ring、删除旧 staging/直拷 | 保留 | 否 |
| P2：外部 TCP/UDP、DNS、IPv6 demux | 数据面保留；RV64 NET_IRQ 已恢复且更安全 | 否 |
| P3-A：Socket/FileOps 普通 fd 入口 | 已选择性恢复 | 否 |
| P3-B：单 SocketImpl、TCP 单锁 | 保留；generation/connect 比 premerge 更完整 | 否 |
| P3-C：关闭清理、资源上限 | 原有限清理仍在；真正 retained close 本来未完成 | 不是 merge 回退 |
| P4：checksum、IPv4 fragment LRU/TTL、ether 文件拆分 | 保留 | 否 |

这份执行前审计当时的结论是“尚未发现未恢复的 merge 回退”。该结论已被
同日 Gate C 实测和补丁审查修正：祖先 `87ae1d21` 有 LA64 22/22 的明确
成功记录，而当前两条 TCP_CRR 都触发 Zone assertion，因此 LA64 TCP_CRR
仍是未恢复项。后续状态以验收矩阵和 active JSON plan 为准。

这个结论有一个重要限定：07-30 后已经有 RV64 9/9 和大量 host targeted
tests，但没有看到恢复后的 LA64 Git 8/8、双架构 netperf/iperf 22/22 以及
完整 LTP/OSComp 网络矩阵重跑。因此 R1 可以判完成，R2 还要补证。

## 还差哪些模块

### A. R2 证据缺口：必须先确认恢复没有退化

这不是新增代码，而是当前最先要做的工作。

| 证据 | merge 前 | 07-30 后 | 当前缺口 |
|---|---|---|---|
| RV64 Git/DNS/HTTP/HTTPS | 8/8 | 9/9 + IRQ sentinel | 已补 |
| LA64 Git 网络链 | 有历史 8/8 | 仅 build/poll-backed 记录 | 待重跑 |
| netperf/iperf | RV64/LA64 × musl/glibc，单架构 22/22 | 未见 post-merge 全矩阵 | 待重跑 |
| LTP socket split | 历史 split 229/236、native/local 415/423 | 台账早于 merge | 待 verdict-set 对账 |
| OSComp network | 历史 libctest/lmbench/netperf/iperf 证据 | 07-30 主要是 Git/curl | 待分组重跑 |

没有这一步，就只能说“代码看起来恢复且 RV64 主见证通过”，不能说“merge
前全部网络功能已恢复”。

### B. TCP/OFD last-close 生命周期：最高优先级结构欠账

当前显式 `close`、`close_range`、process exit 已有 last-close 路径，但：

- `dup3` 覆盖只接住 `_prev` 后 drop；
- exec `CLOEXEC` 直接移除/drop；
- 二者绕过 `FileOps::on_last_close`；
- last-close 本身 abort/take payload，无法承载 FIN_WAIT/TIME_WAIT；
- `SO_LINGER` 没有 close 消费者。

这组工作决定 TCP 关闭是否具有真实 Linux wire 语义，是下一项代码工作。

### C. 动态 sockopt 与 transport 单一真相

已完成：

- `TCP_NODELAY` live engine 更新；
- `SO_KEEPALIVE` live engine 更新；
- `SO_ERROR` take-and-clear。

仍缺：

- `SO_SNDBUF/SO_RCVBUF` 报告值与固定 64 KiB ring 的一致性；
- live `IP_TTL`/IPv6 hop-limit；
- route/egress-aware `TCP_MAXSEG`；
- MTU 改变后 query 与 wire 行为同步。

这应该在 retained-close 稳定后单独做，不能混入 close 所有权变化。

### D. Readiness/wait 双载体

fd readiness 查询入口已经统一为 `query_fd_ready`，但每个 socket 仍同时拥有：

- `RawQueue`；
- 镜像 `WaitSource`；
- live `io_snapshot`；
- sticky queue bits。

每次 fire 同时通知两侧。它是防丢唤醒的过渡结构，不是最终单一真相。该工作
会触及 reactor/wait 语义，必须单独评审并用 poll/select/epoll/connect/close
压力测试验收。

### E. 动态网络对象生命周期

rtnetlink delete 会撤 namespace/route/runtime 引用，但动态 ifname、device、
registration 和 `EtherIface` 仍通过 `Box::leak` 永久存在。bridge/veth/dummy/
vlan 的 create/delete 不能长期反复使用而无泄漏。

这需要把动态对象从 `&'static` 注册提升为有 owner/lifecycle 的对象模型，
不能用零散 `drop` 修补。

### F. 容量与环境硬编码

- ephemeral port pool 只有 64 个且 cursor 全局；
- per-netns endpoint/listener/connection 表固定 256/128/256；
- TCP cork 固定 1460；
- 非 loopback `TCP_MAXSEG` 仍隐含 MTU 1500/IPv4 40 字节；
- IPv6 和不同 egress MTU 未完全区分。

这些是可扩展性/真实拓扑欠账，不是 merge 回退。

### G. L2/L3 所有权

`link.rs`/`l3.rs` 已文件拆分，但共同 `use super::*` 并直接操作同一个
`EtherIface`。`EtherIface` 同时持 device、ARP、NDP、IPv4 reassembly，L3
还直接调用 L2 `transmit_frame`。

这是较大的所有权重构，应在生命周期和 readiness 稳定后另立计划，不能作为
TCP close 的顺手清理。

### H. 已暴露但未闭合的协议功能

| 功能 | 当前状态 |
|---|---|
| `IP_HDRINCL` | 可 set/get，但 sendto/sendmsg 返回 `EOPNOTSUPP` |
| IPv6 fragmentation | RX 无 reassembly，TX 超 MTU 直接 `EMSGSIZE` |
| NDP hop-limit | checksum 会校验，但 ingress 未保留并强制 hop-limit 255 |
| VLAN | 只有 rtnetlink metadata，tagging data path 为空 |
| 控制面/大套件 | `net.features`、multicast、SCTP/NFS/RPC/net_stress 仍有大量未跑/未支持项 |

这些应按独立 Linux/OSComp/LTP witness 切片实现，不应该合成一个“网络栈恢复”
大提交。

## 当前唯一执行路线

### Phase 0：冻结 post-merge 验收基线

目标：回答“当前是否真的恢复到 merge 前的用户态能力”，不修改网络语义。

工作：

1. 记录当前 HEAD、branch、镜像和工具链。
2. 重跑 07-30 targeted host tests。
3. 重跑 RV64 Git/DNS/HTTP/HTTPS，并要求 NET_IRQ sentinel。
4. 重跑 LA64 对应 Git/网络见证；明确其 poll-backed 限定。
5. 重跑 RV64/LA64 × musl/glibc 的 netperf/iperf 四 lane。
6. 用当前 filtered `libctest-musl:`/`libctest-glibc:` 入口重跑网络 ABI；
   旧 `libctest-network`/`lmbench-network` selector 已退役，不直接使用。
7. 对 LTP socket 六个 split 使用相同 case/verdict 集合对账，区分 kernel failure、
   userspace payload failure、架构不适用和未支持协议。

通过条件：

- 当前已知通过项不退化；
- 所有失败都有“本轮回归/既有基线/夹具/架构不适用”分类；
- 串口日志、judge 输出和命令写入 progress；
- 如果出现 merge 回归，先修回归，不进入 Phase 1。

这是下一轮应该最先做的工作。

## 已搁置的长期路线

以下 Phase 1～8 仅保留为历史研究参考，不是当前计划，也不授权代码修改。
若未来要继续其中任何一项，应由用户重新批准并创建独立计划。

### Phase 1：统一 OFD 最后释放入口

目标：让显式和隐式 fd 释放都经过同一个 begin-close 判决，但暂不改变 TCP
transport 行为。

设计边界：

- 提取单一 OFD release helper；
- 覆盖 `close`、`close_range`、process exit、`dup3` replacement、exec
  `CLOEXEC`；
- 引入明确 `CloseOrigin`，至少区分 explicit、close-range、exit、exec、
  dup-replace；
- fd slot 移除仍是一次 OneShot/commit；
- kind-specific `begin_close` 只执行一次；
- 显式 close 可以取得后续 completion，隐式 close 只能启动后台工作。

先写判决测试，暂不修改 `step_socket_close`。

通过条件：

- dup/fork 最后引用只 begin-close 一次；
- dup3 replacement、CLOEXEC、exit、close_range 不漏 hook；
- 非最后 OFD 引用不关闭；
- pipe/socketpair/普通文件语义不退化；
- 原 socket fdtable 与 Git 9/9 仍通过。

### Phase 2：引入有界 `TcpClosingRegistry`

目标：fd 消失后，网络层仍持有 transport、tuple、generation 和 demux 能力。

最小实现：

- 先保留 `Cap<SocketIdentity>`，不立即抽 `TcpFlow`；
- entry 记录 tuple、generation、close policy、deadline/completion；
- registry 是网络 runtime 显式根，不直接强挂进 `NetNamespacePayload`，避免
  namespace → socket → namespace 环；
- 有固定容量、round-robin cursor、统计和超时降级；
- default close 发起 graceful close 后立即向用户返回；
- linger on + timeout 0 发起 abort/RST，但至少保留到有一次发送机会；
- positive linger 先记录 completion/deadline，本阶段不让 syscall park。

通过条件：

- queued data 在 FIN 前交付；
- FIN/RST 能经过 device TX；
- closing tuple 不提前复用；
- registry 满时有明确、稳定的降级/errno；
- namespace 最终可回收，无 registry 残留。

### Phase 3：delegate POLL/TICK、deadline 与原子 reaper

目标：closing transport 有持续推进和安全回收入口。

工作：

- closing entry 加入 delegate 公平扫描；
- `poll_at()` 和 linger deadline 汇总进 `next_deadline`；
- smoltcp terminal state、RST 发送机会和 deadline 形成明确 reap 判据；
- generation/owner-safe reservation 撤本端 index/bind；
- peer reverse index 不随本端 close 被错误删除；
- TIME_WAIT 期间 tuple 继续保留。

通过条件：

- FIN_WAIT/TIME_WAIT/RST/deadline 均有确定终点；
- reaper 每轮工作有界；
- stale generation 不能删新连接；
- 终态/超时后端口可重新绑定；
- Git/curl/netperf 不退化。

### Phase 4：实现 positive `SO_LINGER`

目标：只让显式 positive-linger close 等待，隐式 close 永远后台化。

在改代码前必须先做 Linux witness，确认：

- `O_NONBLOCK + SO_LINGER`；
- signal interruption；
- timeout 时 close 返回形状；
- fd 在等待前是否已经从 table 消失；
- queued data、RST 和 peer EOF 的顺序。

实现形状：

- fd 移除/begin-close 是 OneShot；
- close syscall 上层 script 根据 completion/deadline 选择 FullDrive wait；
- exit、exec、dup3 replacement 不阻塞；
- completion 不复用 `BROKEN` readiness，因为 FIN 开始不等于关闭完成。

### Phase 5：动态 sockopt 单一真相

逐项处理 buffer、TTL/hop-limit、MSS：

1. 先定义 Linux witness 和 unsupported 策略；
2. 对无法安全 live resize 的 buffer 明确钳制或拒绝，不能只改 getsockopt；
3. TTL/hop-limit 更新 live packet engine；
4. MSS 由 route/egress MTU 和 IP family 推导；
5. 添加 wire-level 行为测试。

### Phase 6：readiness 单一真相

先写设计，不直接删 `RawQueue` 或 `WaitSource`：

- 选择唯一 level truth；
- 明确 subscription、register-recheck、sticky edge 的职责；
- 保持 `query_fd_ready` 为 fd-neutral facade；
- 用 poll/select/epoll/connect/close/fcntl 压力测试证明无 lost wake；
- 通过后才删除镜像载体。

### Phase 7：动态对象生命周期与容量配置

拆成两个子计划：

1. bridge/veth/dummy/vlan/ifname/registration/`EtherIface` 的 owner 和回收；
2. per-netns port allocator、table growth/ceiling、MTU/MSS/cork 参数化。

不要在同一提交同时改变 rtnetlink ABI、device ownership 和 TCP 算法。

### Phase 8：L2/L3 和协议功能切片

这是 R3 的长期部分，不是 R1/R2 的完成前提。

建议顺序：

1. NDP hop-limit 255；
2. `IP_HDRINCL`；
3. IPv6 fragmentation/reassembly；
4. VLAN tagged data path；
5. L2 neighbor/frame 与 L3 route/fragment 所有权拆分；
6. 根据 LTP/OSComp 证据再扩展 multicast、control plane 或其他协议。

每项一个 witness、一个语义 owner、一个可回滚提交系列。

## 长期路线原拟验收矩阵

本节只适用于未来重新批准的 Phase 1～8，不适用于当前 merge 恢复停止线。
当前停止线见 `2026-07-31-premerge-network-recovery-test-matrix.md`。

| 层 | 目的 | 典型见证 |
|---|---|---|
| L0 | 编译和格式 | `cargo fmt --check`、相关 crate check/build |
| L1 | 语义单测 | tcp lifecycle、external connect、veth、socket fdtable、substrate |
| L2 | 真实 guest 主链 | `TX_REQUIRE_NET_IRQ=1 tools/verify-git-net.sh`、curl marker |
| L3 | OSComp benchmark | filtered libc-test network ABI、netperf/iperf 四 lane；完整 lmbench 只作附加证据 |
| L4 | Linux 兼容性 | focused LTP socket cases与固定 verdict-set 对账 |

功能成功不能替代机制见证。例如 Git 9/9 不能证明 `NET_IRQ` 工作，因为 10 ms
watchdog 也可能让 Git 通过；必须同时检查 claim/completion 统计。

## 后续执行纪律

1. 每个 phase 只处理一个结构主题，单独提交。
2. phase 开始前记录基线，结束后记录 exact command、日志和 verdict。
3. 自动验证完成后暂停，由用户决定是否进入下一 phase。
4. 不允许以“测试太慢”为理由只跑新增单测并跳过已知网络见证。
5. 不允许为了过 LTP/OSComp 按测试名、argv、端口或 payload 硬编码。
6. 不允许把既有非网络失败归为网络回归，也不允许用“既有失败”掩盖新失败。
7. 每个 phase 更新本计划、`docs/progress/STATUS.md` 和相关 research/debug log。
8. 大重构前先说明最小替代补丁为何不足、影响模块和回滚方式。

## 明确不做的事情

- 不整树回滚到 `90939012`。
- 不机械恢复旧 PLIC mask/complete 顺序。
- 不删除 netlink、bootstrap、64 KiB staging、socket ABI/ioctl 等必要特判。
- 不在 IRQ 证据稳定前删除 10 ms watchdog。
- 不在 retained-close 第一阶段同时抽取 `TcpFlow`、重写 demux 和 procfs。
- 不把 L2/L3、动态对象、IPv6 fragment、VLAN 混成一个“大网络清理”提交。
- 不把历史 premerge 的 benchmark 分数当作当前 post-merge 通过证据。

## 当前阻塞与决策点

Phase 0 没有代码 blocker。

进入 Phase 2 前需要确认 smoltcp close state 的 terminal/reap 判据。
进入 Phase 4 前需要 Linux positive linger、`O_NONBLOCK` 和信号语义 witness。
这两项应由测试/源码证据回答，不能凭经验猜测。

## 参考

- `docs/progress/research/2026-07-30-network-refactor-merge-structural-audit.md`
- `docs/progress/research/2026-07-30-net-irq-restoration-design.md`
- `docs/progress/research/2026-07-30-tcp-retained-close-design.md`
- `msp/debug-logs/2026-07-30-git-clone-fsync-dns-fork-repair.md`
- `msp/debug-logs/2026-07-30-net-irq-deferred-completion.md`
- `docs/LTP/ltp-network-syscall-progress.md`
- `docs/LTP/runtests/ltp-runtest-network-progress.md`
- `docs/Txv3/01_CONCEPTS_v5.md`
- `docs/Txv3/02_INVARIANTS_v5.md`
- `docs/Txv3/03_STEP_MODEL_v2.md`
- `docs/design/00_meta-framework/object_model_v2.md`
