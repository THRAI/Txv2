# LTP 完整测试内容目录（镜像 /musl/ltp/runtest，2026-06-07）

**全集 4056 用例 / 67 个测试集**(网络 934 + 非网络 3122)。

> **OSComp 项目的 `ltp-batch:submit` 白名单只取 `syscalls`(1411),已收录 626。** 其余测试集多需本内核暂无的特性(真块设备/cgroup/命名空间/完整网络/大页/AIO),不在竞赛 ltp suite 内。

> LTP `default` 场景含 30 个集(syscalls/fs/mm/ipc/sched/dio/containers/controllers/hugetlb/commands/cve…),是标准 LTP 全量,非 OSComp 范围。


## ① syscalls(OSComp 跑的工作集)  —— 1 集, 1411 用例
- `syscalls`: 1411

## ② 网络(需完整网络栈,OSComp 未跑)  —— 19 集, 934 用例
- `net.nfs`: 113
- `net_stress.ipsec_udp`: 106
- `net_stress.ipsec_tcp`: 104
- `net_stress.ipsec_sctp`: 104
- `net_stress.ipsec_dccp`: 104
- `net_stress.ipsec_icmp`: 86
- `net.features`: 62
- `net.rpc_tests`: 51
- `net.tirpc_tests`: 41
- `net.sctp`: 41
- `net_stress.interface`: 25
- `net_stress.multicast`: 24
- `net.tcp_cmds`: 17
- `net_stress.route`: 14
- `net_stress.broken_ip`: 11
- `net.ipv6`: 11
- `net_stress.appl`: 10
- `net.ipv6_lib`: 6
- `net.multicast`: 4

## ③ 文件系统/IO(需真块设备/AIO)  —— 12 集, 764 用例
- `scsi_debug.part1`: 140
- `ltp-aiodio.part1`: 140
- `fs_bind`: 95
- `ltp-aiodio.part2`: 83
- `fs`: 68
- `ltp-aiodio.part4`: 59
- `fs_readonly`: 55
- `ltp-aio-stress`: 54
- `dio`: 30
- `ltp-aiodio.part3`: 21
- `fs_perms_simple`: 18
- `fcntl-locktests`: 1

## ④ 内存(大页/NUMA)  —— 3 集, 148 用例
- `mm`: 77
- `hugetlb`: 51
- `numa`: 20

## ⑤ IPC  —— 2 集, 63 用例
- `syscalls-ipc`: 57
- `ipc`: 6

## ⑥ 进程/调度/线程  —— 3 集, 23 用例
- `sched`: 13
- `pty`: 9
- `nptl`: 1

## ⑦ 容器/cgroup(需命名空间/cgroup)  —— 2 集, 431 用例
- `controllers`: 347
- `containers`: 84

## ⑧ 安全/CVE  —— 5 集, 127 用例
- `cve`: 91
- `tpm_tools`: 12
- `smack`: 10
- `ima`: 9
- `capability`: 5

## ⑨ 驱动/硬件/内核子系统  —— 15 集, 75 用例
- `crypto`: 10
- `watchqueue`: 9
- `tracing`: 9
- `dma_thread_diotest`: 7
- `input`: 6
- `cpuhotplug`: 6
- `power_management_tests_exclusive`: 5
- `power_management_tests`: 5
- `kvm`: 5
- `uevent`: 3
- `crashme`: 3
- `can`: 3
- `hyperthreading`: 2
- `s390x_tests`: 1
- `irq`: 1

## ⑩ 命令(需 busybox/coreutils)  —— 1 集, 37 用例
- `commands`: 37

## ⑪ 其他  —— 4 集, 43 用例
- `kernel_misc`: 18
- `smoketest`: 15
- `math`: 10
- `staging`: 0
