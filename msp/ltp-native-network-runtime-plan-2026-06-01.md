# LTP native network runtime plan 2026-06-01

## 当前结论

目标 witness 是：

```text
ltp-runtest:net.tcp_cmds:ipneigh01_ip
```

目前 ARP/neigh 语义 blocker 已经基本关闭，测试能进入 `ipneigh01.sh` 的
native `network.sh` 初始化和后续 stress loop。新的 120 秒 trace 证明：

- 单条 `/tx-ltp/bin/ip` 调用大约 1 秒级，不像是邻居表或路由表线性扫描导致。
- 更大的空洞出现在 `tst_ns_create`、`tst_ns_exec`、远端 `sh -c`、`sysctl`、
  `cat /proc/sys/net/ipv6/...`、多次小命令启动之间。
- 之前 trap-trace 的 syscall 分布也支持这个判断：热点是 `read`、`ppoll`、
  `close`、`wait4`、`dup3`、`clone`、`execve` 和页面 fault，不是 socket 或
  rtnetlink 调用本身。

所以现在不能继续按“网络栈数据结构太慢”去改。下一阶段要按运行时路径处理：
先把每个 helper 命令的耗时测清楚，再决定优化点。

## Phase 1：把耗时定位到命令级别

目标：不改变默认测试行为，只在 `LTP_TRACE_RUNTIME=1` 时收集更细日志。

要做：

- 保留当前 `TX-LTP-RUNTIME begin/end` 和 `/tx-ltp/bin/ip` shim trace。
- 新增仅 trace 模式使用的 wrapper 路径，例如 `/tx-ltp/trace-bin`，默认
  `PATH` 不包含它。
- 在 trace 模式下包装这些命令：
  `tst_check_drivers`、`tst_ns_create`、`tst_ns_exec`、`tst_ns_ifmove`、
  `cat`、`cut`、`grep`、`id`、`ln`、`mkdir`、`mount`、`ping`、`readlink`、
  `sysctl`。
- 每个 wrapper 只打印 begin/end、退出码和 `date +%s`，然后 exec 原命令。

验证：

```sh
timeout 120s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:ipneigh01_ip \
  LTP_TRACE_RUNTIME=1 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-commandtrace-120s.txt
```

预期产物：

- 能回答初始化 120 秒具体花在 `tst_ns_exec`、`sysctl`、`ping` 还是
  `execve/page fault`。
- 如果 wrapper 自身明显拖慢，只保留最小 wrapper 集。

## Phase 2：低风险局部加速

只有 Phase 1 证明某个路径反复慢，才做这里的优化。

候选项：

- 给 `sysctl -qw net.ipv6.conf.<iface>.accept_dad=0` 和
  `cat /proc/sys/net/ipv6/conf/<iface>/disable_ipv6` 做轻量兼容路径。
- 检查 IPv4-only `net.tcp_cmds` 是否可以在本地 runner 上显式跳过无关 IPv6
  初始化；这只能作为本地开发加速开关，不能替代真正的 `net.ipv6*` 覆盖。
- 如果 `tst_ns_exec ... sh -c` 占比最高，再评估是否能减少远端 shell 层数；
  不能跳过 namespace 语义。

验证：

- focused witness 先跑 120s，看是否比现在走得更远。
- 如果进入 stress loop，再升到 300s 或 420s。
- 不用长 timeout 掩盖问题。

## Phase 3：通用运行时优化

如果 Phase 1 显示每个小命令都慢，而不是某一个 helper 慢，就进入通用运行时。

候选项：

- 重复 `execve` 的可执行文件元数据/页缓存路径。
- shell 管道里的 ready pipe `read`/`ppoll`/`wait4` 快路径继续收敛。
- `clone`/`exec` 后的 CLOEXEC、fd table、procfs command metadata 更新成本。
- ext4 热读路径和 page fault 批量化，确认 main 的 cold-read 修复是否已经被
  feature 分支完整继承。

验证：

- 先用 trap-trace 或 commandtrace 证明 syscall/fault 数下降。
- 再跑 `ipneigh01_ip` focused witness。
- 最后回归已经通过的 `net.tcp_cmds` 子集：`netstat`、`iproute`、
  `ping01+ping02`、`arping01`。

## 下一步

先实现 Phase 1 的 trace-only command wrapper，不动默认 runner 行为。拿到
`commandtrace-120s` 后，再决定是做局部 helper 加速，还是进入通用 `execve`
/page-fault 路径优化。

## 2026-06-01 执行结果

Phase 1 已实现，默认路径不包含 `/tx-ltp/trace-bin`，只有
`LTP_TRACE_RUNTIME=1` 时才包装命令。新增 wrapper 覆盖
`tst_check_drivers`、`tst_ns_create`、`tst_ns_exec`、`tst_ns_ifmove`、
`cat`、`cut`、`grep`、`id`、`ln`、`mkdir`、`mount`、`ping`、`ping6`、
`readlink`、`sysctl`。同时默认 `/tx-ltp/bin/tst_check_drivers` 对
`bridge`、`dummy`、`veth` 快速返回成功，其他 driver 仍走 LTP 原 helper。

关键 witness：

```sh
timeout 300s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:ipneigh01_ip \
  LTP_TRACE_RUNTIME=1 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-commandtrace-loop-300s.txt
```

trace 覆盖到了 stress loop。循环内的粗略统计是：

- `ping`: 7 次，总计约 1 秒。
- `ip`: loop 内 18 次，总计约 39 秒，单次最多约 4 秒。
- `grep`: loop 内 12 次，总计约 15 秒，单次最多约 2 秒。

所以当前瓶颈不是 ARP/neighbor 表线性扫描，也不是 ping/ICMP/ARP 数据面慢；
它主要是 `ip neigh show | grep`、`ip neigh del` 这类 shell pipeline 和重复
小进程执行成本。一个 `wait4` post-reap yield batching 实验在 host wait4 tests
中通过，但 300s/420s focused witness 仍没有 PASS，因此没有保留。

下一步不要继续堆 timeout。要么设计更通用的 shell/exec/page-fault 优化，要么
明确做“IPv4-only native network setup 不跑无关 IPv6 初始化”的开发加速路径，
但 IPv6 覆盖必须继续由 `net.ipv6*` 单独证明。
