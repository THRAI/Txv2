# Merge 前网络功能恢复验收矩阵

日期：2026-07-31

状态：Gate A～E 已按序执行并完成审查；恢复仍被 LA64 TCP_CRR 和 LTP b6
证据冲突阻塞。

对比基线：

- merge 前 feature 锚点：`90939012`
- 发生回退的 merge：`6d41a347`
- 制定本矩阵时的 current 锚点：`ad115d68`

## 目标与停止条件

本轮只回答一个问题：

> 当前分支是否保留或恢复了 `90939012` 已经具备并有证据支持的网络功能？

Phase 1～8 的 OFD release、retained close、positive `SO_LINGER`、readiness
收敛、动态对象回收、IPv6 fragmentation、VLAN 和 L2/L3 重构全部搁置，
不作为本轮通过条件。

完成以下 Gate A～E，并且没有相对 merge 前新增的失败，即可结束本轮恢复。
如果出现失败，先分类为本轮回归、既有基线、测试镜像/夹具、用户态 wrapper
或架构不适用；只有确认是本轮回归后，才另建最小修复计划。

## 执行结果（2026-07-31）

本轮发现并修复了两个可直接归因于 merge 的回退，并补了一项当前路径所需
的 Linux UDP 兼容语义：

1. OSComp musl/glibc lane 根目录和动态解释器兼容接线丢失；
2. musl libc-test dynamic lane 从 `runtest.exe` 启动器退化为直接执行
   `libc.so`，产生 `Not a valid dynamic program`。

此外，当前 netperf 路径要求零长度 UDP datagram 作为结束信号。`90939012`
源码本身也会在若干 UDP 路径丢弃空 datagram，因此这里修复的是 Linux 语义
和当前重构后的生产路径，不应伪称为“恢复 anchor 中已有的实现”。
当前 `RawUdpSocket::recv_available()` 仍按 payload 字节数计算；队列中只有
零长度 datagram 时，`can_recv()` 与字节计数可能给出不同的 readiness 视图。
本轮已验证直接收发和 netperf 终止报文，但没有借此启动已搁置的 Phase 6/7
readiness/容量收敛，故将其保留为显式残余风险。

最终证据：

| Gate | 结果 |
|---|---|
| A | 审查后复验：IRQ 7/7、reactor 8/8、fdtable 103/103、TCP lifecycle 27/27、external connect 14/14、veth 4/4、network tick 4/4 |
| B | 审查后复验：RV64 Git/IRQ 9/9，IRQ claims/completions 80/80；LA64 Git/DNS/HTTP/HTTPS 8/8；HTTP/HTTPS clone 使用本地 hostname，SLIRP DNS 另有 30 秒局部超时 |
| C RV64 | netperf/iperf musl+glibc 22/22 |
| C LA64 | iperf 12/12；两条 netperf lane 均先通过四项，再在 TCP_CRR 失败，即 20/22 benchmark points |
| D | musl 12/12；glibc 8/12，只有允许的四项 resolver 历史失败 |
| E | b1、b2、b5 达到可复现目标；b3、b4 与同镜像/runner 的 `90939012` verdict-set 一致；b6 未关闭 |

补充仓库级校验：`cargo -q xtask unit` 未全绿。`tx-shims` 有三个与本轮
网络恢复改动不相交、定向复跑仍失败的既有用例（signal、execve、AF_UNIX
socketpair），`tx-ext4` test target 还有八处 `BackendPageRequest::with_target`
API 编译不一致。它们不改变 Gate A 的网络定向结果，也不在本轮授权范围内，
但不能把全仓 unit gate 写成通过。

审查后的阻塞判定：

- 祖先 `87ae1d21` 明确记录 LA64 musl/glibc 四组 22/22。一次
  `90939012` replay 的 TCP_CRR `Out of memory` 只能说明该次运行也撞到容量
  边界，不能推翻已有成功见证。当前两个 TCP_CRR 的 Zone assertion 都是未恢复项。
- LTP b3、b4 的 current/anchor verdict-set 一致。b6 只对
  `setsockopt06` 做过一次同样 stall 的 anchor replay，未重放完整 anchor tail；
  历史 focused ledger 又有 `setsockopt06` 1/1，因此 Gate E 仍未关闭。

完整命令、日志和每次停止/修复决策记录在
`msp/debug-logs/2026-07-31-premerge-network-recovery-operation-ledger.md`。

## 执行前提

1. 所有命令必须使用同一个待验收 HEAD。
2. 每次 QEMU 运行保留独立串口日志，不覆盖历史日志。
3. Git 验收使用 `local-images/` 下的独立 Alpine 镜像副本。
4. OSComp/LTP 正式验收前必须确认
   `target/oscomp/testdata/sdcard-rv.img` 是干净镜像。当前工作区已有该共享
   RV64 镜像 block-bitmap checksum 损坏记录；不能把它产生的 ext4 失败
   归为网络回归。
5. LTP 使用镜像副本运行；不要让多条 lane 共享一个可写 ext4。
6. 不使用 `cargo xtask oscomp test --suite ...` 选择启动组；`--suite`
   只过滤最终评分。使用 `OSCOMP_GROUPS=...` 或
   `cargo xtask oscomp qemu --boot-suite ...`。

## Gate A：宿主定向回归

这些测试快速证明昨天恢复的 IRQ、FileOps、TCP lifecycle、跨 netns 和
network tick 基本路径仍然成立。

```sh
cargo test -p tx-kernel --lib irq::tests -- --test-threads=1
cargo test -p tx-reactor --lib -- --test-threads=1
cargo test -p tx-shims --lib socket_fdtable -- --test-threads=1
cargo test -p tx-subsystems --lib tcp_lifecycle -- --test-threads=1
cargo test -p tx-subsystems --lib external_connect_tests -- --test-threads=1
cargo test -p tx-subsystems --lib veth_tests -- --test-threads=1
cargo test -p tx-subsystems --lib network_tick -- --test-threads=1
```

预期基线：

| 见证 | 当前已记录基线 |
|---|---:|
| IRQ focused | 7 项 |
| Socket fdtable | 102/102 |
| TCP lifecycle | 27/27 |
| External connect | 14/14 |
| Veth | 4/4 |
| Network tick | 4/4 |

判定：

- 全部通过才进入 Gate B。
- 测试名称匹配出的实际数量若因当前树新增测试而增加，以“零失败”为准。
- broad `net` filter 若碰到已记录的 bridge/epoch 锁中毒，必须单独分类，
  不能直接当作 merge 网络回归。

## Gate B：双架构 Git/DNS/HTTP/HTTPS

### RV64

脚本默认读取 release ELF，因此先显式构建：

```sh
cargo xtask build --target rv64-qemu --release
TX_REQUIRE_NET_IRQ=1 bash tools/verify-git-net.sh
```

通过条件：`9/9`。

前八项是 Git binary、local commit、file content、HTTP clone、HTTPS clone、
push、pull 和 DNS。第九项是机制见证：

- `claims > 0`
- `claims == completions`
- `wrong-hart == 0`
- `missing-device == 0`

只看到 Git 成功但没有 IRQ 统计，不算 RV64 NET_IRQ 恢复通过，因为 10 ms
watchdog 也可能让功能表面通过。

脚本最后打印原始日志路径：
`/tmp/verifygit-XXXXXX/serial.log`。

### LA64

LA64 脚本默认读取 debug ELF，并且脚本的缺失内核自动构建路径误写成 RV64，
所以必须先显式构建 LA64：

```sh
cargo xtask build --target la64-qemu
bash tools/verify-git-net-la64.sh
```

通过条件：`8/8`。

LA64 当前是 poll-backed 网络路径，不要求 RV64 `NET_IRQ` sentinel。
脚本最后同样打印 `/tmp/verifygit-XXXXXX/serial.log`。

## Gate C：双架构、双 libc 的 netperf/iperf

RV64 可以在一次 boot 中选择四个当前有效组。当前 LA64 QEMU `fw_cfg`
会把未转义逗号解析成选项；为避免启动前失败，LA64 按组分别运行：

```sh
timeout 900s make oscomp-local-rv64 \
  OSCOMP_GROUPS=netperf-musl,iperf-musl,netperf-glibc,iperf-glibc \
  OSCOMP_OUT_RV=target/oscomp/recovery-netbench-rv64.txt

timeout 300s make oscomp-local-la64 OSCOMP_GROUPS=netperf-musl \
  OSCOMP_OUT_LA=target/oscomp/recovery-netperf-musl-la64.txt
timeout 300s make oscomp-local-la64 OSCOMP_GROUPS=iperf-musl \
  OSCOMP_OUT_LA=target/oscomp/recovery-iperf-musl-la64.txt
timeout 300s make oscomp-local-la64 OSCOMP_GROUPS=netperf-glibc \
  OSCOMP_OUT_LA=target/oscomp/recovery-netperf-glibc-la64.txt
timeout 300s make oscomp-local-la64 OSCOMP_GROUPS=iperf-glibc \
  OSCOMP_OUT_LA=target/oscomp/recovery-iperf-glibc-la64.txt
```

每个架构的 merge 前基线：

| 组 | 目标 |
|---|---:|
| `netperf-musl` | 5/5 |
| `iperf-musl` | 6/6 |
| `netperf-glibc` | 5/5 |
| `iperf-glibc` | 6/6 |
| 单架构合计 | 22/22 |

双架构总计 44 个 benchmark points。任一架构失败时，再把该架构拆成 musl
和 glibc 两次 boot 定位；不要先重跑四条完整 lane。

netperf 五项是 UDP_STREAM、TCP_STREAM、UDP_RR、TCP_RR、TCP_CRR；
iperf 六项是 BASIC/PARALLEL/REVERSE × UDP/TCP。

## Gate D：聚焦 libc-test 网络 ABI

旧的 `libctest-network` 已经不是当前有效组名。使用当前 filtered group：

```sh
timeout 300s cargo xtask oscomp qemu \
  --target rv64-qemu \
  --boot-suite \
  'libctest-musl:inet_pton+socket+dn_expand_empty+dn_expand_ptr_0+inet_ntop_v4mapped+inet_pton_empty_last_field,libctest-glibc:inet_pton+socket+dn_expand_empty+dn_expand_ptr_0+inet_ntop_v4mapped+inet_pton_empty_last_field'
```

运行后保存 `target/oscomp/os_serial_out_rv.txt` 为独立 recovery 日志。

merge 前判据：

- musl static/dynamic 六项：12/12。
- glibc：不低于 8/12。
- glibc 允许的四个历史失败只能是 static/dynamic 的
  `dn_expand_empty` 和 `dn_expand_ptr_0`。
- 如果 glibc 当前达到 12/12，记为提升，不改变恢复基线。

旧 `lmbench-network` 窄 selector 也已经退役。完整 `lmbench-musl` 含大量
非网络项目，因此不作为恢复硬门槛。需要附加证据时，可以运行当前完整
`lmbench-musl`，只核对 `lat_udp`、`lat_tcp`、`lat_connect`、`bw_tcp`
和 control-channel marker；不得因无关 lmbench 项失败而扩大本轮范围。

## Gate E：LTP socket 六分批固定 verdict-set

这 50 个 case 来自 LTP `runtest/syscalls`，不是上游完整 `net.*`。
不要使用 `LTP_BATCH=net`，当前普通 batch 会把网络前缀过滤成 0 active。

先生成当前 HEAD 的 RV64 submit artifact：

```sh
cargo xtask build --target rv64-qemu
cargo xtask oscomp submit --target rv64-qemu --submit target/oscomp/submit
```

然后使用
`tools/ltp-runtest-witness.sh`。脚本为每条 lane 复制独立镜像并保存
`target/oscomp/ltp-runtest/<tag>-<lane>.{log,judge}`。

### b1：basic socket/listen/options

```sh
bash tools/ltp-runtest-witness.sh 180 rv.musl \
  'socket01+socket02+listen01+getsockname01+getsockopt01+getsockopt02+setsockopt01' \
  recovery-b1
```

目标：`40/40`。

### b2：basic send/recv

```sh
bash tools/ltp-runtest-witness.sh 360 rv.musl \
  'send01+send02+sendto01+sendto02+sendto03+recv01+recvfrom01' \
  recovery-b2
```

目标：`35/35`。

### b3：msg/mmsg

```sh
LTP_BIN_EXTRA_CMDLINE='tx.ltp.max_runtime=10 tx.ltp.max_runtime_cases=sendmsg03' \
bash tools/ltp-runtest-witness.sh 300 rv.musl \
  'sendmsg01+sendmsg02+sendmsg03+recvmsg01+recvmsg02+recvmsg03+sendmmsg01+sendmmsg02+recvmmsg01' \
  recovery-b3
```

目标：`37/38`。musl `recvmmsg01` wrapper SIGSEGV 是历史用户态问题；需要
确认时用 glibc `recvmmsg01` 的 10/10 见证，不要修改内核伪造 musl 通过。

### b4：bind/connect/accept

```sh
bash tools/ltp-runtest-witness.sh 600 rv.musl \
  'bind01+bind02+bind03+bind04+bind05+bind06+connect01+connect02+accept01+accept02+accept03+accept4_01+getpeername01' \
  recovery-b4
```

目标：`93/95`。`connect02` 有 1000 次循环，是本矩阵中允许直接使用 600 秒
上限的已知慢 case。

### b5：socketpair/socketcall

```sh
bash tools/ltp-runtest-witness.sh 180 rv.musl \
  'socketpair01+socketpair02+socketcall01+socketcall02+socketcall03' \
  recovery-b5
```

目标：`14/17`。`socketcall01..03` 是 RV64 不存在的 legacy ABI，不要求为
236/236 给 RV64 添加伪 syscall。

### b6：setsockopt tail

```sh
LTP_BIN_EXTRA_CMDLINE='tx.ltp.max_runtime=30 tx.ltp.max_runtime_cases=setsockopt06' \
bash tools/ltp-runtest-witness.sh 300 rv.musl \
  'setsockopt02+setsockopt03+setsockopt04+setsockopt05+setsockopt06+setsockopt07+setsockopt08+setsockopt09+setsockopt10' \
  recovery-b6
```

目标：`10/11`。`setsockopt03` 的缺项是 32-bit compat-only。

六批总目标：`229/236`。

不能只比较总分，还要比较 `TPASS/TFAIL/TBROK/TCONF/TWARN` 集合。允许保持
相同的历史缺项，不允许新增差项。focused 日志与六批有重叠，不得重复累加。

## 超时和失败处理

上面列出的 aggregate 上限来自已知历史运行时间。若某批失败或静默：

1. 缩小到第一个失败的单 case。
2. 单 case 从外层 `30s` 开始。
3. 有前进输出才依次升到 `60s`、`120s`、`300s`。
4. 记录命令、日志路径、最后一个 `RUN LTP CASE`、judge 分数和 timeout。
5. 有 trap/panic 时运行：

```sh
cargo xtask fault-decode \
  --target rv64-qemu \
  --serial target/oscomp/<descriptive-log>.txt
```

不能通过无限延长 timeout 把 hang 当成通过。

## 明确排除

以下内容不作为 merge 恢复硬门槛：

- Phase 1～8 的所有长期重构；
- 完整 `net.sctp`、NFS、RPC/TIRPC；
- `net.features`、完整 `net.ipv6`、multicast、IPsec；
- netfilter/iptables/nft 完整实现；
- CAN、虚拟化/驱动扩展、远端物理机网络；
- 为得到 LTP 236/236 而实现 RV64 不存在的 legacy `socketcall`；
- 恢复已经退役的 `lmbench-network` test-only selector。

六个 LTP split 中已存在的 SCTP、RDS、最小 netfilter compatibility 子项只要求
保持 merge 前相同 verdict；这不授权扩展完整协议。

## 最终恢复判定

恢复判据如下。直接 anchor replay 用于分类单次失败，但不能覆盖另一个祖先
提交已经保存的成功见证：

1. Gate A 零失败。
2. RV64 Git/IRQ 9/9，LA64 Git 8/8。
3. RV64 和 LA64 netperf/iperf 均达到历史 22/22。
4. libc-test musl 12/12；glibc 不低于 8/12，且失败集合不扩大。
5. LTP 六批优先达到历史汇总；若无法复现，必须运行完整的同命令 anchor
   batch，并解释与任何更早 focused pass 的冲突。
6. 所有日志、命令、HEAD、镜像来源和 blocker 写入 progress。

当前第 3、5 条未满足，所以恢复计划保持 blocked。达到全部条件后也不自动
进入任何后续网络架构工作。
