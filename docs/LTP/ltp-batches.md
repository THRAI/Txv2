# LTP Batch Plan

`tools/ltp-batches.py` 当前只从 OSComp sdcard 的 `/musl/ltp/runtest/syscalls` 列表里按前缀分组。这个文档记录当前 syscalls 子集的划分方式和建议推进顺序。

`runtest/syscalls` 之外的 LTP 原生模块已经单独记录在
`docs/LTP/ltp-runtests.md`，运行入口是
`make oscomp-local-rv64-ltp-runtest LTP_RUNTEST=<module>`。

## Inventory

当前本地 RV LTP 包的规模：

| Item | Count | Meaning |
| --- | ---: | --- |
| runtest files | 67 | LTP 的分组文件数量 |
| runtest entries total | 4056 | 所有 runtest 文件里的入口总数，包含重复入口 |
| unique case names | 3933 | 去重后的入口名数量 |
| testcases/bin binaries | 2820 | 实际可执行文件/脚本数量 |
| syscalls entries | 1411 | `/musl/ltp/runtest/syscalls` 子集入口数 |

注意：下方 `p0`、`smoke`、`fd-io`、`vfs` 等 batch 都是从 `syscalls entries` 这 1411 个入口里切出来的，不等于完整 `ltp-musl-rv`。榜单里的 `ltp-musl-rv` 分数会覆盖更多 runtest 分组，而且一个 LTP 入口可能包含多个 `TPASS/TFAIL/TBROK/TCONF/TWARN` 细项，所以总分可以远大于入口数。

网络/socket 是一个特殊项：`runtest/syscalls` 里实际有 50 个网络/socket
入口，但当前 `tools/ltp-batches.py` 的普通 batch 视图会把这些前缀过滤掉，
所以 `make ltp-batches` 里 `net` 显示为 0。这个 0 表示“当前普通 batch 不会自动跑
网络 case”，不是“LTP 没有网络 case”。网络 bring-up 时应使用
`OSCOMP_LTP=socket01,sendto01,...` 手动点名，具体列表见
`ltp-network-deferred.md`。

`fd-io` 的 host 侧计数仍显示完整 257 个 syscalls 条目；guest 侧内核批次临时跳过 `fcntl15/fcntl15_64`、`fcntl36/fcntl36_64` 和 `pipe02`。`fcntl15` 当前会在 checkpoint 上等到 10s 超时，`fcntl36` 的 OFD/POSIX 锁压力测试目前会卡住整批，`pipe02` 会在 checkpoint wait/wake 上超时后触发测试超时。后续完善对应同步/锁语义后再恢复。

## Current Batches

| Batch | Cases | Scope | Priority | Note |
| --- | ---: | --- | --- | --- |
| p0 | 64 | timer / epoll / eventfd / futex / poll / select | done/maintenance | 当前已重点推进，见 `ltp-progress.md` |
| smoke | 33 | 基础 libc/syscall 小用例 | high | 先跑，通常快且容易涨分 |
| fd-io | 257 | fd、read/write、pipe、fcntl、ioctl、sync 等 | high | 当前进度见 `ltp-fd-io-progress.md`；内核侧暂缓 `fcntl15/fcntl15_64`、`fcntl36/fcntl36_64` 和 `pipe02` |
| vfs | 258 | open/stat/link/rename/xattr/mkdir 等文件系统接口 | high | 面广，适合拆小批跑 |
| vfs-tail | 150 | VFS 从 `link01` 到 `utimes01` 的后半段 | high | 用来接续 VFS，避免超长 `OSCOMP_LTP` 被截断 |
| vm | 103 | brk/mmap/munmap/mprotect/mremap 等内存管理 | high | 很可能暴露 VM 真实问题 |
| process | 109 | clone/fork/exec/wait/pid/session/process_vm 等 | high | 和线程/进程模型强相关 |
| cred | 131 | uid/gid/groups/cap/key 等权限身份接口 | medium | 很多可能是权限模型/占位行为 |
| signal | 51 | kill/sigaction/sigprocmask/sigsuspend/signalfd 等 | medium | 会牵动信号语义，放在 process 后 |
| time | 55 | clock/timer/nanosleep/gettimeofday 等 | low | p0 已做过一轮，剩余多是复杂时间语义 |
| ipc | 67 | SysV IPC / POSIX mq | medium | 可能有成片收益，也可能缺模块 |
| event | 90 | epoll/eventfd/futex/inotify/fanotify/userfaultfd 等 | medium | p0 已覆盖一部分，剩余按模块推进 |
| net | 0 active / 50 deferred | socket/network | manual | 普通 batch 会过滤网络前缀；网络 bring-up 用 `OSCOMP_LTP=...` 点名运行，另见 `ltp-network-deferred.md` |
| sched | 64 | sched/prctl/rlimit/priority/ioprio 等 | medium | 适合 process/signal 后推进 |
| mount | 53 | mount/chroot/unshare/setns/swap/module/reboot 等 | low | namespace/mount/module 类较重 |
| heavy | 67 | bpf/perf/ptrace/quotactl/sysinfo/uname 等 | low | 混合重功能，后置 |
| aio | 15 | io_setup/io_submit/io_getevents/io_uring | low | AIO/io_uring 暂时低优先级 |
| all | 1411 | 全部 syscalls 子集 case | diagnostic only | 不建议日常直接跑；不是完整 LTP |

## Suggested Order

优先顺序：

1. `smoke`
2. `fd-io`
3. `vfs`
4. `vm`
5. `process`
6. `cred`
7. `signal`
8. `ipc`
9. `sched`
10. `event`
11. `time`
12. `mount`
13. `heavy`
14. `aio`

这个顺序的目标是先找高性价比缺口：基础 syscall、文件系统、VM、进程模型通常比 namespace、AIO、hugepage、网络更容易成片推进。

## Commands

查看当前批次和数量：

```bash
make ltp-batches
```

查看某个批次有哪些 case：

```bash
make ltp-batch-cases LTP_BATCH=smoke
```

运行某个批次：

```bash
make oscomp-local-rv64-ltp-batch LTP_BATCH=smoke
```

如果担心卡住，可以加外层超时：

```bash
timeout 300s make oscomp-local-rv64-ltp-batch LTP_BATCH=smoke
```

Makefile 会先用 `tools/ltp-batches.py` 校验 batch 非空并打印数量，然后只把短名字 `ltp-batch:<batch>` 传给 guest。guest 侧在内核里按 `p0` 同样的方式展开硬编码 case 列表，避免长 cmdline 被截断。

当前内核侧已支持 `p0`、`smoke`、`fd-io`、`vfs`、`vfs-tail`、`vm`、`process`、`cred`、`signal`、`time`、`ipc`、`event`、`sched`、`mount`、`heavy` 和 `aio`。`net` 在普通 batch 视图里是 0 个 active case；实际 50 个网络/socket syscall case 当前需要手动点名运行。

大批次仍然建议后续拆成 20 到 40 个 case 的小组，避免一次输出过长，也方便定位卡住的 case。

剩余批次完整命令：

```bash
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=vfs
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=vfs-tail
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=vm
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=process
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=cred
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=signal
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=ipc
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=sched
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=event
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=time
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=mount
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=heavy
timeout 1800s make oscomp-local-rv64-ltp-batch LTP_BATCH=aio
```

## Next Step

先从 `smoke` 开始，跑完记录一个对应的进度文档；如果 `smoke` 大部分通过，再拆 `fd-io` 和 `vfs`。
