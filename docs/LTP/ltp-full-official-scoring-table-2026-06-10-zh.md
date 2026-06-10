# LTP 全量官方计分总表（宿主机 Linux 实测）— 2026-06-10
## 方法与口径

官方评测镜像（pre-20250615，已核实为预赛最新版）`/musl/ltp/testcases/bin/`
全部 2822 个文件，在宿主机 Linux 6.14 上按官方方式逐个**无参执行**
（同版本 LTP 20240930 原生构建；隔离沙箱 = userns+netns+mountns+pidns +
systemd 笼子 MemoryMax=2G/TasksMax=1024；330s 超时帽；LTP_COLORIZE_OUTPUT=y），
每个日志分别喂给 **judge_ltp-musl.py 和 judge_ltp-glibc.py 原版本体**打分。
分数 = musl judge 的 Summary passed 累加（glibc judge 逐文件几乎全同，
仅 7 个文件有小差，见下）。复测工具:`tools/ltp-host-ceiling/`。

## 总计

| 指标 | 值 |
|---|---|
| 实测文件 | 2821（镜像 2822，`prctl04` 二进制不属于 LTP 20240930，宿主无法构建） |
| **算分文件** | **1028** |
| **musl judge 总分** | **8594** |
| **glibc judge 总分** | **8574** |
| 两 judge 有差的文件 | 7（clock_adjtime01/02、clock_settime02、execle01、execve01/06、fs_bind_move12.sh — exec/计时类着色丢失，musl 口径更高） |
| 撞 330s 超时帽 | 28 个文件 |
| 沙箱测不准（按 0 记，真实上限>0，需真 root 复测） | ~165：146 需 loop 块设备（fs/mount 类）、16 需写全局 sysctl（oom/vm 类）、3 需加载内核模块 |
| 官方 0 分但本地形态会打 TPASS 的 legacy 类 | 257 个文件（如 prot_hsymlinks 本地 396、rt_sigaction01-03 本地各 150 —— **官方两 judge 均 0**） |

⚠️ 解读注意：本表是"真 Linux 天花板"。txKernel 能拿多少还受 TCG 速度、
官方 ~300s 预算、QEMU 设备配置约束。网络子集的逐项行动表见
`msp/ltp-net-progress-table-2026-06-10-zh.md`。

## 算分测试（1028 个，按分数降序）

| 测试名 | 算分 | 真实分数 | Linux 耗时 |
|---|---|---|---|
| `splice07` | ✅ | 431 | 0.2s |
| `if-mtu-change.sh` | ✅ | 396 | 34.2s |
| `epoll_ctl03` | ✅ | 256 | 0.0s |
| `memfd_create01` | ✅ | 157 | 0.0s |
| `waitpid01` | ✅ | 146 | 2.9s |
| `getpid01` | ✅ | 100 | 0.1s |
| `route-change-dst.sh` | ✅ | 100 | 6.8s |
| `route-change-gw.sh` | ✅ | 100 | 6.7s |
| `route-change-if.sh` | ✅ | 100 | 8.3s |
| `access01` | ✅ | 98 | 0.1s |
| `pipe11` | ✅ | 70 | 0.0s |
| `tar_tests.sh` | ✅ | 68 | 0.4s |
| `clock_getres01` | ✅ | 44 | 0.0s |
| `fs_bind11.sh` | ✅ | 44 | 0.5s |
| `fs_bind_rbind11.sh` | ✅ | 44 | 0.1s |
| `fs_bind_cloneNS06.sh` | ✅ | 43 | 0.2s |
| `fs_bind09.sh` | ✅ | 40 | 0.2s |
| `fs_bind_rbind09.sh` | ✅ | 40 | 0.1s |
| `fs_bind07.sh` | ✅ | 39 | 0.3s |
| `fs_bind_rbind07.sh` | ✅ | 39 | 0.1s |
| `fs_bind05.sh` | ✅ | 38 | 0.3s |
| `fs_bind_rbind05.sh` | ✅ | 38 | 0.1s |
| `fs_bind03.sh` | ✅ | 37 | 0.3s |
| `fs_bind_rbind03.sh` | ✅ | 37 | 0.1s |
| `fs_bind_rbind19.sh` | ✅ | 37 | 0.2s |
| `fs_bind_rbind21.sh` | ✅ | 37 | 0.1s |
| `fs_bind_rbind27.sh` | ✅ | 37 | 0.2s |
| `file01.sh` | ✅ | 36 | 0.3s |
| `fs_bind19.sh` | ✅ | 36 | 0.2s |
| `splice08` | ✅ | 36 | 0.1s |
| `timer_settime02` | ✅ | 36 | 0.0s |
| `confstr01` | ✅ | 34 | 0.0s |
| `fs_bind10.sh` | ✅ | 34 | 0.4s |
| `fs_bind12.sh` | ✅ | 34 | 0.4s |
| `fs_bind_rbind10.sh` | ✅ | 34 | 0.1s |
| `fs_bind_rbind12.sh` | ✅ | 34 | 0.1s |
| `fs_bind_rbind37.sh` | ✅ | 34 | 0.2s |
| `mq_timedsend01` | ✅ | 34 | 0.2s |
| `fs_bind21.sh` | ✅ | 33 | 0.1s |
| `fs_bind_rbind33.sh` | ✅ | 33 | 0.2s |
| `chmod01` | ✅ | 32 | 0.0s |
| `fs_bind_rbind25.sh` | ✅ | 32 | 0.1s |
| `fs_bind_rbind38.sh` | ✅ | 32 | 0.2s |
| `posix_fadvise03` | ✅ | 32 | 0.0s |
| `posix_fadvise03_64` | ✅ | 32 | 0.0s |
| `process_vm_readv03` | ✅ | 32 | 0.0s |
| `fs_bind17.sh` | ✅ | 31 | 0.1s |
| `fs_bind18.sh` | ✅ | 31 | 0.1s |
| `fs_bind20.sh` | ✅ | 31 | 0.1s |
| `fs_bind_rbind17.sh` | ✅ | 31 | 0.1s |
| `fs_bind_rbind18.sh` | ✅ | 31 | 0.1s |
| `fs_bind_rbind20.sh` | ✅ | 31 | 0.1s |
| `fs_bind_rbind22.sh` | ✅ | 31 | 0.1s |
| `fs_bind04.sh` | ✅ | 30 | 0.2s |
| `fs_bind_cloneNS05.sh` | ✅ | 30 | 0.1s |
| `fs_bind_rbind23.sh` | ✅ | 30 | 0.1s |
| `fs_bind_rbind24.sh` | ✅ | 30 | 0.1s |
| `fs_bind_rbind26.sh` | ✅ | 30 | 0.1s |
| `fs_bind_rbind28.sh` | ✅ | 30 | 0.1s |
| `getitimer01` | ✅ | 30 | 0.1s |
| `mq_timedreceive01` | ✅ | 30 | 0.2s |
| `signal03` | ✅ | 30 | 0.1s |
| `signal05` | ✅ | 30 | 0.2s |
| `fs_bind01.sh` | ✅ | 29 | 0.3s |
| `fs_bind06.sh` | ✅ | 29 | 0.3s |
| `fs_bind08.sh` | ✅ | 29 | 0.2s |
| `fs_bind_move09.sh` | ✅ | 29 | 0.1s |
| `fs_bind_move11.sh` | ✅ | 29 | 0.1s |
| `fs_bind_rbind04.sh` | ✅ | 29 | 0.1s |
| `fs_bind_rbind06.sh` | ✅ | 29 | 0.1s |
| `fs_bind_rbind08.sh` | ✅ | 29 | 0.1s |
| `fs_bind_rbind31.sh` | ✅ | 29 | 0.1s |
| `fs_bind02.sh` | ✅ | 28 | 0.3s |
| `fs_bind_move03.sh` | ✅ | 28 | 0.1s |
| `fs_bind_move10.sh` | ✅ | 28 | 0.1s |
| `fs_bind_move18.sh` | ✅ | 28 | 0.1s |
| `fs_bind_rbind01.sh` | ✅ | 28 | 0.1s |
| `fs_bind_rbind02.sh` | ✅ | 28 | 0.1s |
| `fs_bind_rbind15.sh` | ✅ | 28 | 0.1s |
| `fs_bind_rbind29.sh` | ✅ | 28 | 0.1s |
| `signal04` | ✅ | 28 | 0.0s |
| `fs_bind15.sh` | ✅ | 27 | 0.3s |
| `fs_bind_move02.sh` | ✅ | 27 | 0.1s |
| `fs_bind_move05.sh` | ✅ | 27 | 0.1s |
| `fs_bind_move12.sh` | ✅ | 27(glibc:26) | 0.1s |
| `fs_bind_cloneNS02.sh` | ✅ | 26 | 0.1s |
| `fs_bind_move01.sh` | ✅ | 26 | 0.1s |
| `fs_bind_move19.sh` | ✅ | 26 | 0.1s |
| `fs_bind_move07.sh` | ✅ | 25 | 0.1s |
| `fs_bind_rbind35.sh` | ✅ | 25 | 0.1s |
| `clock_settime02` | ✅ | 24(glibc:12) | 0.0s |
| `fs_bind13.sh` | ✅ | 24 | 0.4s |
| `fs_bind_move06.sh` | ✅ | 24 | 0.1s |
| `fs_bind_rbind13.sh` | ✅ | 24 | 0.1s |
| `kill11` | ✅ | 24 | 2.1s |
| `select01` | ✅ | 24 | 0.2s |
| `select03` | ✅ | 24 | 0.2s |
| `timer_settime01` | ✅ | 24 | 1.8s |
| `du01.sh` | ✅ | 23 | 0.1s |
| `fs_bind_cloneNS07.sh` | ✅ | 23 | 0.1s |
| `fs_bind_rbind34.sh` | ✅ | 23 | 0.1s |
| `fs_bind22.sh` | ✅ | 22 | 0.1s |
| `fs_bind_rbind30.sh` | ✅ | 22 | 0.1s |
| `fs_bind_rbind32.sh` | ✅ | 22 | 0.1s |
| `fs_bind_rbind36.sh` | ✅ | 22 | 0.1s |
| `getaddrinfo_01` | ✅ | 22 | 0.1s |
| `pidns17` | ✅ | 22 | 0.1s |
| `fs_bind_move04.sh` | ✅ | 21 | 0.1s |
| `if-addr-addlarge.sh` | ✅ | 21 | 4.3s |
| `if-route-addlarge.sh` | ✅ | 21 | 3.8s |
| `if-updown.sh` | ✅ | 21 | 4.1s |
| `select02` | ✅ | 21 | 26.4s |
| `ar01.sh` | ✅ | 20 | 0.5s |
| `fs_bind23.sh` | ✅ | 20 | 0.1s |
| `ppoll01` | ✅ | 20 | 0.3s |
| `setns01` | ✅ | 20 | 0.1s |
| `accept03` | ✅ | 19 | 0.1s |
| `fs_bind14.sh` | ✅ | 19 | 0.4s |
| `fs_bind16.sh` | ✅ | 19 | 0.2s |
| `fs_bind_cloneNS01.sh` | ✅ | 19 | 0.1s |
| `fs_bind_rbind14.sh` | ✅ | 19 | 0.1s |
| `fs_bind_rbind16.sh` | ✅ | 19 | 0.1s |
| `fs_bind_regression.sh` | ✅ | 19 | 0.1s |
| `madvise01` | ✅ | 19 | 0.1s |
| `personality01` | ✅ | 19 | 0.1s |
| `process_vm01` | ✅ | 19 | 0.0s |
| `fs_bind07-2.sh` | ✅ | 18 | 0.3s |
| `fs_bind_move08.sh` | ✅ | 18 | 0.1s |
| `fs_bind_rbind07-2.sh` | ✅ | 18 | 0.1s |
| `llseek03` | ✅ | 18 | 0.0s |
| `prctl02` | ✅ | 18 | 0.0s |
| `readahead01` | ✅ | 18 | 0.1s |
| `setitimer01` | ✅ | 18 | 0.2s |
| `fs_bind_move21.sh` | ✅ | 17 | 0.1s |
| `nm01.sh` | ✅ | 17 | 0.3s |
| `pathconf01` | ✅ | 17 | 0.0s |
| `bind04` | ✅ | 16 | 0.0s |
| `clock_gettime01` | ✅ | 16 | 0.1s |
| `fs_bind_move15.sh` | ✅ | 16 | 0.1s |
| `fs_bind_move22.sh` | ✅ | 16 | 0.1s |
| `getrlimit01` | ✅ | 16 | 0.1s |
| `getrlimit03` | ✅ | 16 | 0.0s |
| `openat201` | ✅ | 16 | 0.0s |
| `semctl07` | ✅ | 16 | 0.1s |
| `fs_bind24.sh` | ✅ | 15 | 0.1s |
| `fs_bind_move14.sh` | ✅ | 15 | 0.1s |
| `fs_bind_move20.sh` | ✅ | 15 | 0.1s |
| `lseek02` | ✅ | 15 | 0.0s |
| `lseek11` | ✅ | 15 | 0.1s |
| `msgrcv07` | ✅ | 15 | 0.1s |
| `bind05` | ✅ | 14 | 0.0s |
| `epoll_pwait03` | ✅ | 14 | 16.7s |
| `fs_bind_move13.sh` | ✅ | 14 | 0.1s |
| `memfd_create02` | ✅ | 14 | 0.0s |
| `mmap04` | ✅ | 14 | 0.1s |
| `msgctl01` | ✅ | 14 | 0.0s |
| `prctl07` | ✅ | 14 | 0.0s |
| `prctl08` | ✅ | 14 | 0.0s |
| `semctl01` | ✅ | 13 | 0.2s |
| `asapi_02` | ✅ | 12 | 1.0s |
| `clock_nanosleep01` | ✅ | 12 | 1.5s |
| `clone302` | ✅ | 12 | 0.0s |
| `fcntl15` | ✅ | 12 | 0.0s |
| `fcntl15_64` | ✅ | 12 | 0.0s |
| `fs_bind_cloneNS04.sh` | ✅ | 12 | 0.1s |
| `fs_bind_move16.sh` | ✅ | 12 | 0.1s |
| `fs_bind_move17.sh` | ✅ | 12 | 0.1s |
| `link04` | ✅ | 12 | 0.1s |
| `ln_tests.sh` | ✅ | 12 | 0.1s |
| `madvise10` | ✅ | 12 | 0.1s |
| `membarrier01` | ✅ | 12 | 0.1s |
| `pidns05` | ✅ | 12 | 2.3s |
| `readlinkat01` | ✅ | 12 | 0.1s |
| `shmctl01` | ✅ | 12 | 0.1s |
| `timerfd01` | ✅ | 12 | 1.1s |
| `times03` | ✅ | 12 | 7.8s |
| `wc01.sh` | ✅ | 12 | 0.2s |
| `close_range02` | ✅ | 11 | 0.0s |
| `fs_bind_rbind39.sh` | ✅ | 11 | 0.1s |
| `futex_wake02` | ✅ | 11 | 0.1s |
| `futex_wake03` | ✅ | 11 | 0.1s |
| `gettid02` | ✅ | 11 | 0.0s |
| `ld01.sh` | ✅ | 11 | 0.3s |
| `mkdir03` | ✅ | 11 | 0.0s |
| `sigtimedwait01` | ✅ | 11 | 1.4s |
| `clock_gettime02` | ✅ | 10 | 0.0s |
| `cp_tests.sh` | ✅ | 10 | 0.2s |
| `inotify10` | ✅ | 10 | 0.1s |
| `madvise02` | ✅ | 10 | 0.1s |
| `ping01.sh` | ✅ | 10 | 4.7s |
| `ping02.sh` | ✅ | 10 | 0.3s |
| `readv01` | ✅ | 10 | 0.1s |
| `recvmmsg01` | ✅ | 10 | 0.1s |
| `recvmsg01` | ✅ | 10 | 0.1s |
| `socketpair01` | ✅ | 10 | 0.1s |
| `waitid08` | ✅ | 10 | 0.0s |
| `add_key02` | ✅ | 9 | 0.1s |
| `binfmt_misc01.sh` | ✅ | 9 | 0.1s |
| `epoll_ctl02` | ✅ | 9 | 0.0s |
| `epoll_wait06` | ✅ | 9 | 0.0s |
| `fpathconf01` | ✅ | 9 | 0.1s |
| `fs_bind_cloneNS03.sh` | ✅ | 9 | 0.1s |
| `futex_waitv01` | ✅ | 9 | 0.1s |
| `getrandom03` | ✅ | 9 | 0.1s |
| `getrusage03` | ✅ | 9 | 1.2s |
| `getsockopt01` | ✅ | 9 | 0.0s |
| `inotify02` | ✅ | 9 | 0.0s |
| `inotify12` | ✅ | 9 | 0.1s |
| `ioctl01` | ✅ | 9 | 0.0s |
| `ioctl03` | ✅ | 9 | 0.0s |
| `memfd_create04` | ✅ | 9 | 0.0s |
| `name_to_handle_at02` | ✅ | 9 | 0.0s |
| `openat202` | ✅ | 9 | 0.0s |
| `openat203` | ✅ | 9 | 0.0s |
| `rmdir02` | ✅ | 9 | 0.2s |
| `sigwaitinfo01` | ✅ | 9 | 0.4s |
| `socket01` | ✅ | 9 | 0.3s |
| `vlan01.sh` | ✅ | 9 | 4.2s |
| `accept4_01` | ✅ | 8 | 0.1s |
| `access02` | ✅ | 8 | 0.1s |
| `execve05` | ✅ | 8 | 0.0s |
| `fallocate03` | ✅ | 8 | 0.0s |
| `fchmod01` | ✅ | 8 | 0.0s |
| `getpgid01` | ✅ | 8 | 0.1s |
| `mlock201` | ✅ | 8 | 0.0s |
| `mmap06` | ✅ | 8 | 0.1s |
| `prctl05` | ✅ | 8 | 0.0s |
| `preadv02` | ✅ | 8 | 0.0s |
| `preadv02_64` | ✅ | 8 | 0.0s |
| `preadv202` | ✅ | 8 | 0.1s |
| `preadv202_64` | ✅ | 8 | 0.0s |
| `sched_setparam04` | ✅ | 8 | 0.1s |
| `sched_setscheduler01` | ✅ | 8 | 0.1s |
| `sched_setscheduler04` | ✅ | 8 | 0.0s |
| `semop03` | ✅ | 8 | 0.1s |
| `setsockopt01` | ✅ | 8 | 0.1s |
| `timens01` | ✅ | 8 | 0.1s |
| `writev07` | ✅ | 8 | 0.0s |
| `bind01` | ✅ | 7 | 0.1s |
| `clock_nanosleep02` | ✅ | 7 | 8.4s |
| `clone301` | ✅ | 7 | 0.0s |
| `cve-2022-4378` | ✅ | 7 | 0.0s |
| `epoll_wait02` | ✅ | 7 | 8.4s |
| `faccessat201` | ✅ | 7 | 0.0s |
| `fcntl36` | ✅ | 7 | 7.2s |
| `fcntl36_64` | ✅ | 7 | 7.2s |
| `futex_cmp_requeue01` | ✅ | 7 | 0.7s |
| `futex_wait05` | ✅ | 7 | 8.8s |
| `getpeername01` | ✅ | 7 | 0.1s |
| `inotify01` | ✅ | 7 | 0.0s |
| `io_submit03` | ✅ | 7 | 0.1s |
| `mq_notify01` | ✅ | 7 | 0.0s |
| `mq_notify03` | ✅ | 7 | 0.0s |
| `msgrcv02` | ✅ | 7 | 0.1s |
| `nanosleep01` | ✅ | 7 | 9.0s |
| `pipe2_01` | ✅ | 7 | 0.0s |
| `poll02` | ✅ | 7 | 8.4s |
| `prctl09` | ✅ | 7 | 8.6s |
| `pselect01` | ✅ | 7 | 8.4s |
| `pselect01_64` | ✅ | 7 | 8.5s |
| `pwritev02` | ✅ | 7 | 0.1s |
| `pwritev02_64` | ✅ | 7 | 0.1s |
| `pwritev202` | ✅ | 7 | 0.1s |
| `pwritev202_64` | ✅ | 7 | 0.0s |
| `setreuid01` | ✅ | 7 | 0.1s |
| `signalfd01` | ✅ | 7 | 0.1s |
| `splice03` | ✅ | 7 | 0.2s |
| `statx03` | ✅ | 7 | 0.4s |
| `unlinkat01` | ✅ | 7 | 0.1s |
| `unshare01.sh` | ✅ | 7 | 0.2s |
| `userns03` | ✅ | 7 | 0.1s |
| `access04` | ✅ | 6 | 0.1s |
| `alarm02` | ✅ | 6 | 0.1s |
| `broken_ip-checksum.sh` | ✅ | 6 | 15.1s |
| `broken_ip-dstaddr.sh` | ✅ | 6 | 15.1s |
| `broken_ip-fragment.sh` | ✅ | 6 | 15.1s |
| `broken_ip-ihl.sh` | ✅ | 6 | 15.1s |
| `broken_ip-plen.sh` | ✅ | 6 | 14.6s |
| `broken_ip-protcol.sh` | ✅ | 6 | 14.7s |
| `broken_ip-version.sh` | ✅ | 6 | 14.8s |
| `capget01` | ✅ | 6 | 0.0s |
| `capset02` | ✅ | 6 | 0.0s |
| `clock_adjtime02` | ✅ | 6(glibc:3) | 0.0s |
| `clock_gettime04` | ✅ | 6 | 0.1s |
| `creat01` | ✅ | 6 | 0.0s |
| `dup202` | ✅ | 6 | 0.0s |
| `fchmodat01` | ✅ | 6 | 0.0s |
| `fchmodat02` | ✅ | 6 | 0.0s |
| `fcntl02` | ✅ | 6 | 0.0s |
| `fcntl02_64` | ✅ | 6 | 0.0s |
| `fcntl05` | ✅ | 6 | 0.0s |
| `fcntl05_64` | ✅ | 6 | 0.0s |
| `flock04` | ✅ | 6 | 0.1s |
| `fstat02` | ✅ | 6 | 0.0s |
| `fstat02_64` | ✅ | 6 | 0.1s |
| `futex_wake01` | ✅ | 6 | 0.1s |
| `getsockname01` | ✅ | 6 | 0.0s |
| `kcmp02` | ✅ | 6 | 0.1s |
| `pidns30` | ✅ | 6 | 0.1s |
| `pidns31` | ✅ | 6 | 0.1s |
| `pipe12` | ✅ | 6 | 0.0s |
| `posix_fadvise01` | ✅ | 6 | 0.0s |
| `posix_fadvise01_64` | ✅ | 6 | 0.0s |
| `posix_fadvise02` | ✅ | 6 | 0.0s |
| `posix_fadvise02_64` | ✅ | 6 | 0.0s |
| `posix_fadvise04` | ✅ | 6 | 0.0s |
| `posix_fadvise04_64` | ✅ | 6 | 0.0s |
| `prctl03` | ✅ | 6 | 0.1s |
| `preadv201` | ✅ | 6 | 0.0s |
| `preadv201_64` | ✅ | 6 | 0.1s |
| `pwritev201` | ✅ | 6 | 0.1s |
| `pwritev201_64` | ✅ | 6 | 0.1s |
| `readlinkat02` | ✅ | 6 | 0.1s |
| `request_key03` | ✅ | 6 | 1.5s |
| `sched_get_priority_max01` | ✅ | 6 | 0.0s |
| `sched_get_priority_min01` | ✅ | 6 | 0.1s |
| `sched_getparam03` | ✅ | 6 | 0.1s |
| `select04` | ✅ | 6 | 0.1s |
| `semctl03` | ✅ | 6 | 0.2s |
| `shmctl08` | ✅ | 6 | 1.0s |
| `signal01` | ✅ | 6 | 0.3s |
| `statfs02` | ✅ | 6 | 0.2s |
| `statfs02_64` | ✅ | 6 | 0.2s |
| `sysinfo03` | ✅ | 6 | 0.3s |
| `tgkill03` | ✅ | 6 | 0.0s |
| `timer_delete01` | ✅ | 6 | 0.1s |
| `timerfd02` | ✅ | 6 | 0.1s |
| `uevent02` | ✅ | 6 | 0.1s |
| `unlink07` | ✅ | 6 | 0.1s |
| `waitid05` | ✅ | 6 | 0.1s |
| `waitid06` | ✅ | 6 | 0.1s |
| `writev01` | ✅ | 6 | 0.0s |
| `accept01` | ✅ | 5 | 0.1s |
| `broken_ip-nexthdr.sh` | ✅ | 5 | 14.9s |
| `capget02` | ✅ | 5 | 0.0s |
| `chroot03` | ✅ | 5 | 0.0s |
| `clone08` | ✅ | 5 | 0.0s |
| `creat06` | ✅ | 5 | 0.0s |
| `epoll_wait03` | ✅ | 5 | 0.0s |
| `epoll_wait07` | ✅ | 5 | 0.0s |
| `eventfd02` | ✅ | 5 | 0.0s |
| `faccessat202` | ✅ | 5 | 0.0s |
| `fsync03` | ✅ | 5 | 0.0s |
| `getcwd01` | ✅ | 5 | 0.0s |
| `in6_01` | ✅ | 5 | 0.0s |
| `inotify04` | ✅ | 5 | 0.0s |
| `io_setup02` | ✅ | 5 | 0.1s |
| `ip_tests.sh` | ✅ | 5 | 0.3s |
| `kcmp01` | ✅ | 5 | 0.1s |
| `llseek01` | ✅ | 5 | 0.0s |
| `mallopt01` | ✅ | 5 | 0.1s |
| `mkdir_tests.sh` | ✅ | 5 | 0.1s |
| `msgsnd02` | ✅ | 5 | 0.0s |
| `open07` | ✅ | 5 | 0.0s |
| `openat01` | ✅ | 5 | 0.1s |
| `pivot_root01` | ✅ | 5 | 0.0s |
| `prctl10` | ✅ | 5 | 0.1s |
| `pwrite02` | ✅ | 5 | 0.0s |
| `pwrite02_64` | ✅ | 5 | 0.0s |
| `read02` | ✅ | 5 | 0.1s |
| `readv02` | ✅ | 5 | 0.1s |
| `sched_rr_get_interval03` | ✅ | 5 | 0.1s |
| `semget02` | ✅ | 5 | 0.1s |
| `sendfile04` | ✅ | 5 | 0.0s |
| `sendfile04_64` | ✅ | 5 | 0.1s |
| `setregid01` | ✅ | 5 | 0.1s |
| `setregid04` | ✅ | 5 | 0.1s |
| `statvfs02` | ✅ | 5 | 0.2s |
| `statx02` | ✅ | 5 | 0.3s |
| `sync_file_range01` | ✅ | 5 | 0.1s |
| `umip_basic_test` | ✅ | 5 | 0.0s |
| `utime07` | ✅ | 5 | 0.0s |
| `vxlan01.sh` | ✅ | 5 | 2.3s |
| `waitid01` | ✅ | 5 | 0.0s |
| `waitid07` | ✅ | 5 | 0.1s |
| `waitid10` | ✅ | 5 | 0.2s |
| `waitid11` | ✅ | 5 | 0.1s |
| `access03` | ✅ | 4 | 0.1s |
| `add_key01` | ✅ | 4 | 0.1s |
| `arch_prctl01` | ✅ | 4 | 0.0s |
| `clock_nanosleep04` | ✅ | 4 | 0.1s |
| `dup201` | ✅ | 4 | 0.0s |
| `dup203` | ✅ | 4 | 0.0s |
| `dup204` | ✅ | 4 | 0.0s |
| `epoll_create01` | ✅ | 4 | 0.0s |
| `epoll_create02` | ✅ | 4 | 0.0s |
| `epoll_pwait01` | ✅ | 4 | 0.1s |
| `eventfd01` | ✅ | 4 | 0.0s |
| `execveat01` | ✅ | 4 | 0.0s |
| `execveat02` | ✅ | 4 | 0.0s |
| `fcntl13` | ✅ | 4 | 0.0s |
| `fcntl13_64` | ✅ | 4 | 0.0s |
| `fcntl30` | ✅ | 4 | 0.0s |
| `fcntl30_64` | ✅ | 4 | 0.0s |
| `fcntl39` | ✅ | 4 | 0.0s |
| `fcntl39_64` | ✅ | 4 | 0.0s |
| `flock06` | ✅ | 4 | 0.1s |
| `ftruncate03` | ✅ | 4 | 0.0s |
| `ftruncate03_64` | ✅ | 4 | 0.0s |
| `futex_wait01` | ✅ | 4 | 0.0s |
| `getpriority02` | ✅ | 4 | 0.1s |
| `getrandom01` | ✅ | 4 | 0.1s |
| `getrandom02` | ✅ | 4 | 0.1s |
| `getxattr01` | ✅ | 4 | 0.1s |
| `inotify_init1_01` | ✅ | 4 | 0.1s |
| `inotify_init1_02` | ✅ | 4 | 0.0s |
| `ioctl_ns07` | ✅ | 4 | 0.1s |
| `kcmp03` | ✅ | 4 | 0.0s |
| `keyctl09` | ✅ | 4 | 0.0s |
| `link08` | ✅ | 4 | 0.0s |
| `lseek01` | ✅ | 4 | 0.0s |
| `macvlan01.sh` | ✅ | 4 | 2.0s |
| `macvtap01.sh` | ✅ | 4 | 2.1s |
| `mkdirat02` | ✅ | 4 | 0.0s |
| `mlock01` | ✅ | 4 | 0.1s |
| `mmap18` | ✅ | 4 | 0.0s |
| `mq_open01` | ✅ | 4 | 0.0s |
| `msgrcv01` | ✅ | 4 | 0.0s |
| `munlock01` | ✅ | 4 | 0.0s |
| `pathconf02` | ✅ | 4 | 0.0s |
| `pidfd_getfd02` | ✅ | 4 | 10.1s |
| `pidns02` | ✅ | 4 | 0.1s |
| `pidns16` | ✅ | 4 | 0.1s |
| `pipe13` | ✅ | 4 | 0.1s |
| `ptrace01` | ✅ | 4 | 0.1s |
| `remap_file_pages02` | ✅ | 4 | 0.2s |
| `sched_getaffinity01` | ✅ | 4 | 0.1s |
| `sched_getparam01` | ✅ | 4 | 0.1s |
| `sched_rr_get_interval01` | ✅ | 4 | 0.1s |
| `send02` | ✅ | 4 | 0.1s |
| `sendfile01.sh` | ✅ | 4 | 0.7s |
| `sendfile03` | ✅ | 4 | 0.1s |
| `sendfile03_64` | ✅ | 4 | 0.1s |
| `sendmmsg01` | ✅ | 4 | 0.1s |
| `sendmmsg02` | ✅ | 4 | 0.0s |
| `setns02` | ✅ | 4 | 0.1s |
| `setpriority02` | ✅ | 4 | 0.1s |
| `shmat01` | ✅ | 4 | 0.1s |
| `shmctl03` | ✅ | 4 | 0.1s |
| `shmctl07` | ✅ | 4 | 0.1s |
| `sigpending02` | ✅ | 4 | 0.2s |
| `socket02` | ✅ | 4 | 0.3s |
| `socketpair02` | ✅ | 4 | 0.1s |
| `sysfs05` | ✅ | 4 | 0.1s |
| `timerfd04` | ✅ | 4 | 0.3s |
| `timerfd_settime01` | ✅ | 4 | 0.1s |
| `userns06` | ✅ | 4 | 0.1s |
| `waitpid04` | ✅ | 4 | 0.0s |
| `waitpid09` | ✅ | 4 | 0.0s |
| `alarm05` | ✅ | 3 | 2.1s |
| `bind03` | ✅ | 3 | 0.0s |
| `capset01` | ✅ | 3 | 0.0s |
| `chdir04` | ✅ | 3 | 0.0s |
| `close01` | ✅ | 3 | 0.0s |
| `cpio_tests.sh` | ✅ | 3 | 0.1s |
| `dup07` | ✅ | 3 | 0.0s |
| `dup3_02` | ✅ | 3 | 0.0s |
| `epoll_ctl01` | ✅ | 3 | 0.0s |
| `epoll_pwait05` | ✅ | 3 | 0.0s |
| `epoll_wait01` | ✅ | 3 | 0.0s |
| `eventfd03` | ✅ | 3 | 0.0s |
| `eventfd04` | ✅ | 3 | 0.0s |
| `faccessat01` | ✅ | 3 | 0.0s |
| `fcntl29` | ✅ | 3 | 0.0s |
| `fcntl29_64` | ✅ | 3 | 0.0s |
| `fcntl37` | ✅ | 3 | 0.1s |
| `fcntl37_64` | ✅ | 3 | 0.1s |
| `flock01` | ✅ | 3 | 0.1s |
| `flock02` | ✅ | 3 | 0.1s |
| `flock03` | ✅ | 3 | 0.0s |
| `fork04` | ✅ | 3 | 0.1s |
| `futex_cmp_requeue02` | ✅ | 3 | 0.1s |
| `getcwd02` | ✅ | 3 | 0.1s |
| `getitimer02` | ✅ | 3 | 0.1s |
| `getpriority01` | ✅ | 3 | 0.1s |
| `getrusage02` | ✅ | 3 | 0.1s |
| `gettimeofday01` | ✅ | 3 | 0.0s |
| `in6_02` | ✅ | 3 | 0.1s |
| `ioprio_set02` | ✅ | 3 | 0.0s |
| `ioprio_set03` | ✅ | 3 | 0.0s |
| `iptables01.sh` | ✅ | 3 | 7.5s |
| `ipvlan01.sh` | ✅ | 3 | 1.1s |
| `keyctl05` | ✅ | 3 | 0.1s |
| `kill03` | ✅ | 3 | 0.0s |
| `mmap09` | ✅ | 3 | 0.0s |
| `mq_unlink01` | ✅ | 3 | 0.0s |
| `mremap06` | ✅ | 3 | 0.0s |
| `msgctl12` | ✅ | 3 | 0.0s |
| `msgget02` | ✅ | 3 | 0.0s |
| `msgrcv03` | ✅ | 3 | 0.1s |
| `msgsnd01` | ✅ | 3 | 0.0s |
| `nanosleep04` | ✅ | 3 | 0.0s |
| `netns_comm.sh` | ✅ | 3 | 3.2s |
| `netns_sysfs.sh` | ✅ | 3 | 0.1s |
| `pidfd_open02` | ✅ | 3 | 0.0s |
| `pidfd_open04` | ✅ | 3 | 0.0s |
| `pidns06` | ✅ | 3 | 0.1s |
| `pidns10` | ✅ | 3 | 0.0s |
| `pidns12` | ✅ | 3 | 0.1s |
| `pread02` | ✅ | 3 | 0.0s |
| `pread02_64` | ✅ | 3 | 0.0s |
| `preadv01` | ✅ | 3 | 0.0s |
| `preadv01_64` | ✅ | 3 | 0.0s |
| `pselect02` | ✅ | 3 | 0.1s |
| `pselect02_64` | ✅ | 3 | 0.1s |
| `ptrace08` | ✅ | 3 | 0.3s |
| `pwritev01` | ✅ | 3 | 0.1s |
| `pwritev01_64` | ✅ | 3 | 0.0s |
| `request_key02` | ✅ | 3 | 2.4s |
| `sbrk01` | ✅ | 3 | 0.1s |
| `sched_setaffinity01` | ✅ | 3 | 0.1s |
| `semctl05` | ✅ | 3 | 0.1s |
| `semget01` | ✅ | 3 | 0.1s |
| `setitimer02` | ✅ | 3 | 0.1s |
| `setpgid02` | ✅ | 3 | 0.1s |
| `setpgid03` | ✅ | 3 | 0.1s |
| `setreuid02` | ✅ | 3 | 0.1s |
| `settimeofday02` | ✅ | 3 | 0.1s |
| `signal02` | ✅ | 3 | 0.2s |
| `sigwait01` | ✅ | 3 | 0.3s |
| `syscall01` | ✅ | 3 | 0.2s |
| `sysctl02.sh` | ✅ | 3 | 0.1s |
| `tee02` | ✅ | 3 | 0.0s |
| `timer_gettime01` | ✅ | 3 | 0.1s |
| `timerfd_gettime01` | ✅ | 3 | 0.1s |
| `traceroute01.sh` | ✅ | 3 | 0.4s |
| `unshare01` | ✅ | 3 | 0.0s |
| `unzip01.sh` | ✅ | 3 | 0.1s |
| `userns01` | ✅ | 3 | 0.1s |
| `userns05` | ✅ | 3 | 0.0s |
| `vmsplice02` | ✅ | 3 | 0.0s |
| `wait401` | ✅ | 3 | 0.1s |
| `write05` | ✅ | 3 | 0.0s |
| `abort01` | ✅ | 2 | 0.2s |
| `alarm03` | ✅ | 2 | 0.1s |
| `alarm06` | ✅ | 2 | 3.0s |
| `alarm07` | ✅ | 2 | 3.0s |
| `brk01` | ✅ | 2 | 0.0s |
| `brk02` | ✅ | 2 | 0.0s |
| `cgroup_regression_test.sh` | ✅ | 2 | 61.2s |
| `chown02` | ✅ | 2 | 0.0s |
| `chroot02` | ✅ | 2 | 0.0s |
| `clock_adjtime01` | ✅ | 2(glibc:1) | 0.0s |
| `clock_nanosleep03` | ✅ | 2 | 0.2s |
| `clone01` | ✅ | 2 | 0.0s |
| `copy_file_range03` | ✅ | 2 | 3.0s |
| `dup01` | ✅ | 2 | 0.0s |
| `dup02` | ✅ | 2 | 0.0s |
| `dup04` | ✅ | 2 | 0.0s |
| `dup207` | ✅ | 2 | 0.0s |
| `dup3_01` | ✅ | 2 | 0.0s |
| `epoll_create1_01` | ✅ | 2 | 0.0s |
| `epoll_create1_02` | ✅ | 2 | 0.0s |
| `epoll_pwait02` | ✅ | 2 | 0.0s |
| `epoll_pwait04` | ✅ | 2 | 0.0s |
| `eventfd05` | ✅ | 2 | 0.0s |
| `eventfd2_01` | ✅ | 2 | 0.0s |
| `eventfd2_02` | ✅ | 2 | 0.0s |
| `eventfd2_03` | ✅ | 2 | 0.0s |
| `faccessat02` | ✅ | 2 | 0.0s |
| `fchown02` | ✅ | 2 | 0.0s |
| `fcntl14` | ✅ | 2 | 2.4s |
| `fcntl14_64` | ✅ | 2 | 2.3s |
| `fcntl27` | ✅ | 2 | 0.0s |
| `fcntl27_64` | ✅ | 2 | 0.0s |
| `fcntl38` | ✅ | 2 | 0.1s |
| `fcntl38_64` | ✅ | 2 | 0.1s |
| `fork01` | ✅ | 2 | 0.1s |
| `fork10` | ✅ | 2 | 0.1s |
| `fstat03` | ✅ | 2 | 0.1s |
| `fstat03_64` | ✅ | 2 | 0.1s |
| `fstatfs02` | ✅ | 2 | 0.1s |
| `fstatfs02_64` | ✅ | 2 | 0.0s |
| `ftruncate01` | ✅ | 2 | 0.1s |
| `ftruncate01_64` | ✅ | 2 | 0.0s |
| `futex_wait_bitset01` | ✅ | 2 | 0.3s |
| `getcontext01` | ✅ | 2 | 0.0s |
| `geteuid02` | ✅ | 2 | 0.0s |
| `getpgid02` | ✅ | 2 | 0.1s |
| `getpgrp01` | ✅ | 2 | 0.1s |
| `getpid02` | ✅ | 2 | 0.0s |
| `getrlimit02` | ✅ | 2 | 0.1s |
| `getrusage01` | ✅ | 2 | 0.1s |
| `gettid01` | ✅ | 2 | 0.1s |
| `getuid03` | ✅ | 2 | 0.0s |
| `gzip_tests.sh` | ✅ | 2 | 0.7s |
| `io_submit02` | ✅ | 2 | 0.1s |
| `ioctl_ns01` | ✅ | 2 | 0.0s |
| `ioctl_ns05` | ✅ | 2 | 0.0s |
| `keyctl01` | ✅ | 2 | 0.1s |
| `keyctl07` | ✅ | 2 | 0.0s |
| `ldd01.sh` | ✅ | 2 | 0.1s |
| `link02` | ✅ | 2 | 0.1s |
| `linktest.sh` | ✅ | 2 | 1.8s |
| `llseek02` | ✅ | 2 | 0.0s |
| `lseek07` | ✅ | 2 | 0.0s |
| `mallinfo01` | ✅ | 2 | 0.1s |
| `mallinfo02` | ✅ | 2 | 0.1s |
| `mcast-group-multiple-socket.sh` | ✅ | 2 | 0.7s |
| `mcast-group-single-socket.sh` | ✅ | 2 | 0.6s |
| `mcast-queryfld01.sh` | ✅ | 2 | 11.3s |
| `mcast-queryfld02.sh` | ✅ | 2 | 11.6s |
| `mcast-queryfld03.sh` | ✅ | 2 | 11.0s |
| `mcast-queryfld04.sh` | ✅ | 2 | 11.4s |
| `mcast-queryfld05.sh` | ✅ | 2 | 10.6s |
| `mcast-queryfld06.sh` | ✅ | 2 | 10.9s |
| `memcmp01` | ✅ | 2 | 0.0s |
| `memcpy01` | ✅ | 2 | 0.0s |
| `mincore02` | ✅ | 2 | 0.0s |
| `mincore03` | ✅ | 2 | 0.0s |
| `mknod01` | ✅ | 2 | 0.0s |
| `mlock05` | ✅ | 2 | 0.0s |
| `mountns01` | ✅ | 2 | 0.1s |
| `mountns02` | ✅ | 2 | 0.0s |
| `mountns03` | ✅ | 2 | 0.1s |
| `mq_notify02` | ✅ | 2 | 0.0s |
| `msgctl02` | ✅ | 2 | 0.0s |
| `msgctl03` | ✅ | 2 | 0.0s |
| `msgsnd05` | ✅ | 2 | 0.0s |
| `munlockall01` | ✅ | 2 | 0.0s |
| `mv_tests.sh` | ✅ | 2 | 0.3s |
| `nanosleep02` | ✅ | 2 | 1.0s |
| `netns_breakns.sh` | ✅ | 2 | 0.2s |
| `nft01.sh` | ✅ | 2 | 8.4s |
| `open01` | ✅ | 2 | 0.0s |
| `open09` | ✅ | 2 | 0.0s |
| `open_by_handle_at02` | ✅ | 2 | 0.0s |
| `pidfd_send_signal01` | ✅ | 2 | 0.0s |
| `pidns01` | ✅ | 2 | 0.0s |
| `pidns04` | ✅ | 2 | 0.0s |
| `pipe03` | ✅ | 2 | 0.1s |
| `pipe07` | ✅ | 2 | 2.5s |
| `pipe2_04` | ✅ | 2 | 0.0s |
| `poll01` | ✅ | 2 | 0.0s |
| `prctl01` | ✅ | 2 | 0.0s |
| `process_vm_writev02` | ✅ | 2 | 0.0s |
| `ptrace03` | ✅ | 2 | 0.1s |
| `request_key01` | ✅ | 2 | 0.2s |
| `route-change-netlink-dst.sh` | ✅ | 2 | 1.1s |
| `route-change-netlink-gw.sh` | ✅ | 2 | 1.4s |
| `route-change-netlink-if.sh` | ✅ | 2 | 1.5s |
| `rt_sigqueueinfo01` | ✅ | 2 | 0.1s |
| `rt_sigsuspend01` | ✅ | 2 | 1.1s |
| `sched_getscheduler01` | ✅ | 2 | 0.1s |
| `sched_getscheduler02` | ✅ | 2 | 0.1s |
| `sched_setparam01` | ✅ | 2 | 0.1s |
| `sched_setparam02` | ✅ | 2 | 0.1s |
| `semtest_2ns` | ✅ | 2 | 0.1s |
| `sendfile02` | ✅ | 2 | 0.1s |
| `sendfile02_64` | ✅ | 2 | 0.1s |
| `sendfile09` | ✅ | 2 | 5.7s |
| `sendfile09_64` | ✅ | 2 | 4.6s |
| `sendto03` | ✅ | 2 | 0.8s |
| `setpgrp02` | ✅ | 2 | 0.1s |
| `setresuid01` | ✅ | 2 | 0.1s |
| `setrlimit03` | ✅ | 2 | 0.1s |
| `setsockopt02` | ✅ | 2 | 0.3s |
| `shmat02` | ✅ | 2 | 0.0s |
| `shmdt01` | ✅ | 2 | 0.1s |
| `shmdt02` | ✅ | 2 | 0.1s |
| `sigaltstack02` | ✅ | 2 | 0.1s |
| `snd_seq01` | ✅ | 2 | 86.5s |
| `splice09` | ✅ | 2 | 0.1s |
| `stat02` | ✅ | 2 | 0.1s |
| `stat02_64` | ✅ | 2 | 0.1s |
| `symlink04` | ✅ | 2 | 0.1s |
| `test_1_to_1_initmsg_connect` | ✅ | 2 | 0.0s |
| `test_ioctl` | ✅ | 2 | 0.2s |
| `time01` | ✅ | 2 | 0.0s |
| `timer_getoverrun01` | ✅ | 2 | 0.0s |
| `timerfd_create01` | ✅ | 2 | 0.1s |
| `tkill01` | ✅ | 2 | 0.0s |
| `tkill02` | ✅ | 2 | 0.1s |
| `truncate02` | ✅ | 2 | 0.1s |
| `truncate02_64` | ✅ | 2 | 0.1s |
| `uname01` | ✅ | 2 | 0.1s |
| `uname04` | ✅ | 2 | 0.0s |
| `unlink05` | ✅ | 2 | 0.1s |
| `userns02` | ✅ | 2 | 0.1s |
| `userns04` | ✅ | 2 | 0.1s |
| `ustat02` | ✅ | 2 | 0.1s |
| `utsname03` | ✅ | 2 | 0.0s |
| `vmsplice04` | ✅ | 2 | 0.1s |
| `waitid04` | ✅ | 2 | 0.1s |
| `waitpid03` | ✅ | 2 | 0.0s |
| `which01.sh` | ✅ | 2 | 0.1s |
| `write02` | ✅ | 2 | 0.0s |
| `write06` | ✅ | 2 | 0.0s |
| `add_key04` | ✅ | 1 | 0.1s |
| `adjtimex03` | ✅ | 1 | 0.0s |
| `af_alg05` | ✅ | 1 | 0.1s |
| `af_alg06` | ✅ | 1 | 0.1s |
| `af_alg07` | ✅ | 1 | 0.1s |
| `aslr01` | ✅ | 1 | 2.9s |
| `bind06` | ✅ | 1 | 133.6s |
| `capset03` | ✅ | 1 | 0.0s |
| `capset04` | ✅ | 1 | 0.0s |
| `chown01` | ✅ | 1 | 0.0s |
| `chown05` | ✅ | 1 | 0.0s |
| `clone03` | ✅ | 1 | 0.0s |
| `clone04` | ✅ | 1 | 0.0s |
| `clone05` | ✅ | 1 | 0.1s |
| `clone06` | ✅ | 1 | 0.0s |
| `clone07` | ✅ | 1 | 0.0s |
| `clone09` | ✅ | 1 | 0.0s |
| `close02` | ✅ | 1 | 0.0s |
| `connect02` | ✅ | 1 | 0.1s |
| `creat03` | ✅ | 1 | 0.0s |
| `creat05` | ✅ | 1 | 30.7s |
| `crypto_user01` | ✅ | 1 | 0.1s |
| `cve-2014-0196` | ✅ | 1 | 35.8s |
| `cve-2016-10044` | ✅ | 1 | 0.0s |
| `cve-2016-7042` | ✅ | 1 | 0.0s |
| `cve-2016-7117` | ✅ | 1 | 41.8s |
| `cve-2017-16939` | ✅ | 1 | 0.1s |
| `cve-2017-17052` | ✅ | 1 | 1.6s |
| `cve-2017-17053` | ✅ | 1 | 5.0s |
| `cve-2017-2618` | ✅ | 1 | 0.0s |
| `cve-2017-2671` | ✅ | 1 | 8.4s |
| `dio_append` | ✅ | 1 | 0.9s |
| `dio_read` | ✅ | 1 | 0.9s |
| `dio_sparse` | ✅ | 1 | 4.8s |
| `dio_truncate` | ✅ | 1 | 14.5s |
| `dirtypipe` | ✅ | 1 | 0.0s |
| `dup03` | ✅ | 1 | 0.3s |
| `dup05` | ✅ | 1 | 0.0s |
| `dup06` | ✅ | 1 | 0.5s |
| `dup205` | ✅ | 1 | 0.3s |
| `dup206` | ✅ | 1 | 0.0s |
| `epoll_ctl04` | ✅ | 1 | 0.0s |
| `epoll_ctl05` | ✅ | 1 | 0.0s |
| `epoll_wait04` | ✅ | 1 | 0.0s |
| `epoll_wait05` | ✅ | 1 | 0.0s |
| `execl01` | ✅ | 1 | 0.0s |
| `execle01` | ✅ | 1(glibc:0) | 0.0s |
| `execlp01` | ✅ | 1 | 0.0s |
| `execv01` | ✅ | 1 | 0.0s |
| `execve01` | ✅ | 1(glibc:0) | 0.0s |
| `execve06` | ✅ | 1(glibc:0) | 0.0s |
| `execvp01` | ✅ | 1 | 0.0s |
| `exit02` | ✅ | 1 | 0.0s |
| `exit_group01` | ✅ | 1 | 0.1s |
| `fanout01` | ✅ | 1 | 180.0s |
| `fchdir01` | ✅ | 1 | 0.0s |
| `fchdir02` | ✅ | 1 | 0.0s |
| `fchmod04` | ✅ | 1 | 0.0s |
| `fchown01` | ✅ | 1 | 0.0s |
| `fchown05` | ✅ | 1 | 0.0s |
| `fcntl03` | ✅ | 1 | 0.0s |
| `fcntl03_64` | ✅ | 1 | 0.0s |
| `fcntl04` | ✅ | 1 | 0.0s |
| `fcntl04_64` | ✅ | 1 | 0.0s |
| `fcntl08` | ✅ | 1 | 0.0s |
| `fcntl08_64` | ✅ | 1 | 0.0s |
| `fcntl12` | ✅ | 1 | 1.1s |
| `fcntl12_64` | ✅ | 1 | 1.7s |
| `fcntl34` | ✅ | 1 | 0.1s |
| `fcntl34_64` | ✅ | 1 | 0.1s |
| `fgetxattr03` | ✅ | 1 | 0.0s |
| `finit_module02` | ✅ | 1 | 0.0s |
| `fork03` | ✅ | 1 | 0.1s |
| `fork07` | ✅ | 1 | 0.1s |
| `fork08` | ✅ | 1 | 0.1s |
| `fork14` | ✅ | 1 | 0.8s |
| `fork_procs` | ✅ | 1 | 0.8s |
| `fsx-linux` | ✅ | 1 | 1.6s |
| `fsync02` | ✅ | 1 | 0.2s |
| `futex_wait02` | ✅ | 1 | 0.1s |
| `futex_wait03` | ✅ | 1 | 0.0s |
| `futex_wait04` | ✅ | 1 | 0.1s |
| `futex_waitv02` | ✅ | 1 | 0.1s |
| `futex_waitv03` | ✅ | 1 | 0.1s |
| `gdb01.sh` | ✅ | 1 | 0.7s |
| `generate_lvm_runfile.sh` | ✅ | 1 | 0.8s |
| `getcpu01` | ✅ | 1 | 0.0s |
| `getcwd03` | ✅ | 1 | 0.0s |
| `getcwd04` | ✅ | 1 | 5.0s |
| `getdomainname01` | ✅ | 1 | 0.0s |
| `getegid01` | ✅ | 1 | 0.0s |
| `getegid01_16` | ✅ | 1 | 0.0s |
| `getegid02` | ✅ | 1 | 0.0s |
| `getegid02_16` | ✅ | 1 | 0.0s |
| `geteuid01` | ✅ | 1 | 0.0s |
| `getgid03` | ✅ | 1 | 0.1s |
| `gethostbyname_r01` | ✅ | 1 | 0.1s |
| `gethostid01` | ✅ | 1 | 0.1s |
| `gethostname01` | ✅ | 1 | 0.1s |
| `gethostname02` | ✅ | 1 | 0.1s |
| `getpagesize01` | ✅ | 1 | 0.1s |
| `getppid01` | ✅ | 1 | 0.1s |
| `getppid02` | ✅ | 1 | 0.1s |
| `getrandom04` | ✅ | 1 | 0.1s |
| `getsid01` | ✅ | 1 | 0.0s |
| `getsid02` | ✅ | 1 | 0.0s |
| `getsockopt02` | ✅ | 1 | 0.0s |
| `gettimeofday02` | ✅ | 1 | 10.0s |
| `getuid01` | ✅ | 1 | 0.0s |
| `icmp_rate_limit01` | ✅ | 1 | 4.1s |
| `if-addr-adddel.sh` | ✅ | 1 | 0.8s |
| `if-route-adddel.sh` | ✅ | 1 | 0.9s |
| `if4-addr-change.sh` | ✅ | 1 | 0.7s |
| `ima_policy.sh` | ✅ | 1 | 0.1s |
| `inotify05` | ✅ | 1 | 0.1s |
| `inotify09` | ✅ | 1 | 16.4s |
| `inotify11` | ✅ | 1 | 0.4s |
| `io_cancel01` | ✅ | 1 | 0.0s |
| `io_destroy02` | ✅ | 1 | 0.0s |
| `io_getevents01` | ✅ | 1 | 0.0s |
| `ioctl07` | ✅ | 1 | 0.0s |
| `ioctl_ns02` | ✅ | 1 | 0.0s |
| `ioctl_ns03` | ✅ | 1 | 0.0s |
| `ioctl_ns04` | ✅ | 1 | 0.0s |
| `ioctl_ns06` | ✅ | 1 | 0.0s |
| `iopl01` | ✅ | 1 | 0.0s |
| `ioprio_get01` | ✅ | 1 | 0.0s |
| `ioprio_set01` | ✅ | 1 | 0.0s |
| `kallsyms` | ✅ | 1 | 2.3s |
| `keyctl03` | ✅ | 1 | 0.0s |
| `keyctl04` | ✅ | 1 | 0.0s |
| `keyctl06` | ✅ | 1 | 0.0s |
| `keyctl08` | ✅ | 1 | 0.0s |
| `kill06` | ✅ | 1 | 0.0s |
| `lftest` | ✅ | 1 | 1.6s |
| `link05` | ✅ | 1 | 0.1s |
| `lsmod01.sh` | ✅ | 1 | 0.1s |
| `lstat01` | ✅ | 1 | 0.0s |
| `lstat01_64` | ✅ | 1 | 0.0s |
| `madvise03` | ✅ | 1 | 0.1s |
| `madvise05` | ✅ | 1 | 0.0s |
| `mallinfo2_01` | ✅ | 1 | 0.1s |
| `mallocstress` | ✅ | 1 | 2.8s |
| `mcast-group-same-group.sh` | ✅ | 1 | 0.6s |
| `mcast-group-source-filter.sh` | ✅ | 1 | 0.4s |
| `mcast-pktfld01.sh` | ✅ | 1 | 10.7s |
| `mcast-pktfld02.sh` | ✅ | 1 | 11.2s |
| `memset01` | ✅ | 1 | 0.0s |
| `mesgq_nstest` | ✅ | 1 | 0.0s |
| `mincore04` | ✅ | 1 | 0.0s |
| `mknod09` | ✅ | 1 | 0.0s |
| `mlock02` | ✅ | 1 | 0.1s |
| `mlock03` | ✅ | 1 | 0.1s |
| `mlock04` | ✅ | 1 | 0.0s |
| `mlock202` | ✅ | 1 | 0.0s |
| `mlock203` | ✅ | 1 | 0.0s |
| `mmap02` | ✅ | 1 | 0.0s |
| `mmap05` | ✅ | 1 | 0.0s |
| `mmap08` | ✅ | 1 | 0.0s |
| `mmap1` | ✅ | 1 | 180.2s |
| `mmap12` | ✅ | 1 | 0.0s |
| `mmap13` | ✅ | 1 | 0.0s |
| `mmap15` | ✅ | 1 | 0.0s |
| `mmap17` | ✅ | 1 | 0.0s |
| `mmap19` | ✅ | 1 | 0.0s |
| `mmap20` | ✅ | 1 | 0.0s |
| `mmap3` | ✅ | 1 | 60.1s |
| `mmapstress01` | ✅ | 1 | 12.0s |
| `mmapstress04` | ✅ | 1 | 0.0s |
| `mountns04` | ✅ | 1 | 0.0s |
| `mprotect05` | ✅ | 1 | 0.0s |
| `mqns_01` | ✅ | 1 | 0.0s |
| `mqns_02` | ✅ | 1 | 0.0s |
| `msg_comm` | ✅ | 1 | 0.0s |
| `msgget01` | ✅ | 1 | 0.0s |
| `msgrcv05` | ✅ | 1 | 0.1s |
| `msgrcv06` | ✅ | 1 | 0.1s |
| `msgrcv08` | ✅ | 1 | 0.0s |
| `msgsnd06` | ✅ | 1 | 0.1s |
| `mtest01` | ✅ | 1 | 0.0s |
| `munlock02` | ✅ | 1 | 0.0s |
| `netns_netlink` | ✅ | 1 | 0.1s |
| `netstat01.sh` | ✅ | 1 | 0.4s |
| `nft02` | ✅ | 1 | 0.0s |
| `nice02` | ✅ | 1 | 0.0s |
| `nice03` | ✅ | 1 | 0.0s |
| `open03` | ✅ | 1 | 0.0s |
| `open04` | ✅ | 1 | 34.6s |
| `open06` | ✅ | 1 | 0.0s |
| `pause01` | ✅ | 1 | 0.0s |
| `personality02` | ✅ | 1 | 0.0s |
| `pidfd_getfd01` | ✅ | 1 | 0.0s |
| `pidfd_open01` | ✅ | 1 | 0.0s |
| `pidfd_open03` | ✅ | 1 | 0.1s |
| `pidfd_send_signal03` | ✅ | 1 | 0.1s |
| `pidns03` | ✅ | 1 | 0.1s |
| `pidns13` | ✅ | 1 | 0.1s |
| `pidns20` | ✅ | 1 | 0.1s |
| `pidns32` | ✅ | 1 | 0.1s |
| `pipe01` | ✅ | 1 | 0.0s |
| `pipe02` | ✅ | 1 | 0.0s |
| `pipe06` | ✅ | 1 | 1.9s |
| `pipe08` | ✅ | 1 | 0.0s |
| `pipe10` | ✅ | 1 | 0.0s |
| `pipe14` | ✅ | 1 | 0.0s |
| `pipe15` | ✅ | 1 | 0.1s |
| `pipe2_02` | ✅ | 1 | 0.0s |
| `pread01` | ✅ | 1 | 0.0s |
| `pread01_64` | ✅ | 1 | 0.0s |
| `process_vm_readv02` | ✅ | 1 | 0.0s |
| `pselect03` | ✅ | 1 | 0.1s |
| `pselect03_64` | ✅ | 1 | 0.2s |
| `ptrace07` | ✅ | 1 | 1.4s |
| `ptrace09` | ✅ | 1 | 0.2s |
| `ptrace10` | ✅ | 1 | 0.2s |
| `ptrace11` | ✅ | 1 | 0.2s |
| `pty02` | ✅ | 1 | 0.0s |
| `pty05` | ✅ | 1 | 3.5s |
| `pwrite01` | ✅ | 1 | 0.1s |
| `pwrite01_64` | ✅ | 1 | 0.1s |
| `pwrite03` | ✅ | 1 | 0.1s |
| `pwrite03_64` | ✅ | 1 | 0.1s |
| `pwrite04` | ✅ | 1 | 0.0s |
| `pwrite04_64` | ✅ | 1 | 0.1s |
| `read01` | ✅ | 1 | 0.0s |
| `read03` | ✅ | 1 | 0.1s |
| `read04` | ✅ | 1 | 0.1s |
| `readlink01` | ✅ | 1 | 0.1s |
| `realpath01` | ✅ | 1 | 0.1s |
| `reboot02` | ✅ | 1 | 0.0s |
| `recvmsg02` | ✅ | 1 | 0.1s |
| `request_key04` | ✅ | 1 | 0.1s |
| `request_key05` | ✅ | 1 | 0.1s |
| `rmdir01` | ✅ | 1 | 0.1s |
| `route-redirect.sh` | ✅ | 1 | 0.9s |
| `sbrk02` | ✅ | 1 | 0.1s |
| `sched_get_priority_max02` | ✅ | 1 | 0.1s |
| `sched_get_priority_min02` | ✅ | 1 | 0.1s |
| `sctp_big_chunk` | ✅ | 1 | 0.2s |
| `sem_comm` | ✅ | 1 | 0.1s |
| `sem_nstest` | ✅ | 1 | 0.1s |
| `semop04` | ✅ | 1 | 0.4s |
| `sendfile05` | ✅ | 1 | 0.1s |
| `sendfile05_64` | ✅ | 1 | 0.1s |
| `sendfile06` | ✅ | 1 | 0.1s |
| `sendfile06_64` | ✅ | 1 | 0.1s |
| `sendfile07` | ✅ | 1 | 0.1s |
| `sendfile07_64` | ✅ | 1 | 0.1s |
| `sendfile08` | ✅ | 1 | 0.1s |
| `sendfile08_64` | ✅ | 1 | 0.1s |
| `sendmsg03` | ✅ | 1 | 6.6s |
| `sendto02` | ✅ | 1 | 0.1s |
| `setfsgid01` | ✅ | 1 | 0.3s |
| `setfsuid01` | ✅ | 1 | 0.2s |
| `setfsuid02` | ✅ | 1 | 0.1s |
| `setgid01` | ✅ | 1 | 0.1s |
| `setresuid04` | ✅ | 1 | 0.2s |
| `setreuid05` | ✅ | 1 | 0.2s |
| `setreuid07` | ✅ | 1 | 0.1s |
| `setrlimit04` | ✅ | 1 | 0.1s |
| `setrlimit05` | ✅ | 1 | 0.1s |
| `setrlimit06` | ✅ | 1 | 2.1s |
| `setsockopt03` | ✅ | 1 | 0.2s |
| `setsockopt05` | ✅ | 1 | 0.0s |
| `setsockopt06` | ✅ | 1 | 270.4s |
| `setsockopt07` | ✅ | 1 | 98.1s |
| `setsockopt08` | ✅ | 1 | 0.1s |
| `setsockopt09` | ✅ | 1 | 3.7s |
| `setuid01` | ✅ | 1 | 0.1s |
| `shm_comm` | ✅ | 1 | 0.1s |
| `shmat03` | ✅ | 1 | 0.1s |
| `shmat04` | ✅ | 1 | 0.2s |
| `shmctl05` | ✅ | 1 | 10.1s |
| `shmem_2nstest` | ✅ | 1 | 0.0s |
| `shmget03` | ✅ | 1 | 0.1s |
| `shmnstest` | ✅ | 1 | 0.2s |
| `sighold02` | ✅ | 1 | 0.1s |
| `sigsuspend01` | ✅ | 1 | 1.1s |
| `snd_timer01` | ✅ | 1 | 90.2s |
| `splice01` | ✅ | 1 | 0.2s |
| `splice02` | ✅ | 1 | 0.4s |
| `splice04` | ✅ | 1 | 0.1s |
| `splice05` | ✅ | 1 | 0.1s |
| `stack_clash` | ✅ | 1 | 0.3s |
| `starvation` | ✅ | 1 | 28.9s |
| `symlink02` | ✅ | 1 | 0.3s |
| `sysfs01` | ✅ | 1 | 0.2s |
| `sysfs02` | ✅ | 1 | 0.1s |
| `sysfs03` | ✅ | 1 | 0.1s |
| `sysfs04` | ✅ | 1 | 0.1s |
| `tee01` | ✅ | 1 | 0.2s |
| `tgkill01` | ✅ | 1 | 0.1s |
| `tgkill02` | ✅ | 1 | 0.0s |
| `thp01` | ✅ | 1 | 0.2s |
| `thp02` | ✅ | 1 | 0.2s |
| `thp03` | ✅ | 1 | 0.1s |
| `thp04` | ✅ | 1 | 37.4s |
| `timer_delete02` | ✅ | 1 | 0.1s |
| `timer_settime03` | ✅ | 1 | 0.0s |
| `timerfd_settime02` | ✅ | 1 | 39.9s |
| `times01` | ✅ | 1 | 0.0s |
| `tracepath01.sh` | ✅ | 1 | 0.4s |
| `umask01` | ✅ | 1 | 0.1s |
| `uname02` | ✅ | 1 | 0.0s |
| `unlink08` | ✅ | 1 | 0.1s |
| `unshare02` | ✅ | 1 | 0.0s |
| `ustat01` | ✅ | 1 | 0.0s |
| `utsname01` | ✅ | 1 | 0.1s |
| `vlan02.sh` | ✅ | 1 | 0.8s |
| `vma05.sh` | ✅ | 1 | 0.2s |
| `vmsplice01` | ✅ | 1 | 0.1s |
| `vmsplice03` | ✅ | 1 | 0.1s |
| `vsock01` | ✅ | 1 | 60.1s |
| `vxlan02.sh` | ✅ | 1 | 1.0s |
| `wait01` | ✅ | 1 | 0.0s |
| `wait02` | ✅ | 1 | 0.1s |
| `wait402` | ✅ | 1 | 0.1s |
| `wait403` | ✅ | 1 | 0.0s |
| `waitid02` | ✅ | 1 | 0.1s |
| `waitid03` | ✅ | 1 | 0.0s |
| `waitid09` | ✅ | 1 | 0.0s |
| `waitpid06` | ✅ | 1 | 0.1s |
| `waitpid07` | ✅ | 1 | 0.0s |
| `waitpid08` | ✅ | 1 | 0.1s |
| `waitpid10` | ✅ | 1 | 2.1s |
| `waitpid11` | ✅ | 1 | 0.0s |
| `waitpid12` | ✅ | 1 | 0.1s |
| `waitpid13` | ✅ | 1 | 0.1s |
| `wqueue01` | ✅ | 1 | 0.1s |
| `wqueue02` | ✅ | 1 | 0.1s |
| `wqueue03` | ✅ | 1 | 0.0s |
| `wqueue04` | ✅ | 1 | 0.0s |
| `wqueue05` | ✅ | 1 | 0.0s |
| `wqueue06` | ✅ | 1 | 0.1s |
| `wqueue07` | ✅ | 1 | 0.0s |
| `wqueue08` | ✅ | 1 | 0.1s |
| `wqueue09` | ✅ | 1 | 0.1s |
| `write01` | ✅ | 1 | 0.1s |
| `write03` | ✅ | 1 | 0.0s |
| `write04` | ✅ | 1 | 0.0s |

## 不算分测试（1793 个，字母序）

| 测试名 | 算分 | 备注 |
|---|---|---|
| `abs01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `accept02` | ❌ |  |
| `acct01` | ❌ |  |
| `acct02` | ❌ |  |
| `acct02_helper` | ❌ |  |
| `acl1` | ❌ |  |
| `add_ipv6addr` | ❌ |  |
| `add_key03` | ❌ |  |
| `add_key05` | ❌ | 沙箱测不准:需写全局 sysctl |
| `adjtimex01` | ❌ |  |
| `adjtimex02` | ❌ |  |
| `af_alg01` | ❌ |  |
| `af_alg02` | ❌ |  |
| `af_alg03` | ❌ |  |
| `af_alg04` | ❌ |  |
| `aio-stress` | ❌ |  |
| `aio01` | ❌ |  |
| `aio02` | ❌ |  |
| `aiocp` | ❌ |  |
| `aiodio_append` | ❌ |  |
| `aiodio_sparse` | ❌ |  |
| `arping01.sh` | ❌ |  |
| `asapi_01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `asapi_03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ask_password.sh` | ❌ |  |
| `assign_password.sh` | ❌ |  |
| `atof01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `autogroup01` | ❌ |  |
| `bbr01.sh` | ❌ |  |
| `bbr02.sh` | ❌ |  |
| `bind02` | ❌ |  |
| `bind_noport01.sh` | ❌ |  |
| `binfmt_misc02.sh` | ❌ |  |
| `binfmt_misc_lib.sh` | ❌ |  |
| `block_dev` | ❌ |  |
| `bpf_map01` | ❌ |  |
| `bpf_prog01` | ❌ |  |
| `bpf_prog02` | ❌ |  |
| `bpf_prog03` | ❌ |  |
| `bpf_prog04` | ❌ |  |
| `bpf_prog05` | ❌ |  |
| `bpf_prog06` | ❌ |  |
| `bpf_prog07` | ❌ |  |
| `busy_poll01.sh` | ❌ |  |
| `busy_poll02.sh` | ❌ |  |
| `busy_poll03.sh` | ❌ |  |
| `busy_poll_lib.sh` | ❌ |  |
| `cacheflush01` | ❌ |  |
| `can_bcm01` | ❌ | 沙箱测不准:需内核模块 |
| `can_filter` | ❌ | 沙箱测不准:需内核模块 |
| `can_rcv_own_msgs` | ❌ | 沙箱测不准:需内核模块 |
| `cap_bounds_r` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `cap_bounds_rw` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `cap_bset_inh_bounds` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `cfs_bandwidth01` | ❌ |  |
| `cgroup_core01` | ❌ |  |
| `cgroup_core02` | ❌ |  |
| `cgroup_core03` | ❌ |  |
| `cgroup_fj_common.sh` | ❌ |  |
| `cgroup_fj_function.sh` | ❌ |  |
| `cgroup_fj_proc` | ❌ |  |
| `cgroup_fj_stress.sh` | ❌ |  |
| `cgroup_lib.sh` | ❌ |  |
| `cgroup_regression_3_1.sh` | ❌ |  |
| `cgroup_regression_3_2.sh` | ❌ |  |
| `cgroup_regression_5_1.sh` | ❌ |  |
| `cgroup_regression_5_2.sh` | ❌ |  |
| `cgroup_regression_6_1.sh` | ❌ |  |
| `cgroup_regression_6_2.sh` | ❌ |  |
| `cgroup_regression_fork_processes` | ❌ |  |
| `cgroup_regression_getdelays` | ❌ |  |
| `cgroup_xattr` | ❌ |  |
| `change_password.sh` | ❌ |  |
| `chdir01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `check_envval` | ❌ |  |
| `check_icmpv4_connectivity` | ❌ |  |
| `check_icmpv6_connectivity` | ❌ |  |
| `check_keepcaps` | ❌ |  |
| `check_netem` | ❌ |  |
| `check_pe` | ❌ |  |
| `check_setkey` | ❌ |  |
| `check_simple_capset` | ❌ |  |
| `chmod03` | ❌ |  |
| `chmod05` | ❌ |  |
| `chmod06` | ❌ |  |
| `chmod07` | ❌ |  |
| `chown01_16` | ❌ |  |
| `chown02_16` | ❌ |  |
| `chown03` | ❌ |  |
| `chown03_16` | ❌ |  |
| `chown04` | ❌ |  |
| `chown04_16` | ❌ |  |
| `chown05_16` | ❌ |  |
| `chroot01` | ❌ |  |
| `chroot04` | ❌ |  |
| `cleanup_lvm.sh` | ❌ |  |
| `clock_gettime03` | ❌ |  |
| `clock_settime01` | ❌ |  |
| `clock_settime03` | ❌ |  |
| `clone02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `clone303` | ❌ |  |
| `close_range01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `cmdlib.sh` | ❌ |  |
| `cn_pec.sh` | ❌ |  |
| `connect01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `copy_file_range01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `copy_file_range02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `cpuacct.sh` | ❌ |  |
| `cpuacct_task` | ❌ |  |
| `cpuctl_def_task01` | ❌ |  |
| `cpuctl_def_task02` | ❌ |  |
| `cpuctl_def_task03` | ❌ |  |
| `cpuctl_def_task04` | ❌ |  |
| `cpuctl_fj_cpu-hog` | ❌ |  |
| `cpuctl_fj_simple_echo` | ❌ |  |
| `cpuctl_latency_check_task` | ❌ |  |
| `cpuctl_latency_test` | ❌ |  |
| `cpuctl_test01` | ❌ |  |
| `cpuctl_test02` | ❌ |  |
| `cpuctl_test03` | ❌ |  |
| `cpuctl_test04` | ❌ |  |
| `cpufreq_boost` | ❌ |  |
| `cpuhotplug01.sh` | ❌ |  |
| `cpuhotplug02.sh` | ❌ |  |
| `cpuhotplug03.sh` | ❌ |  |
| `cpuhotplug04.sh` | ❌ |  |
| `cpuhotplug05.sh` | ❌ |  |
| `cpuhotplug06.sh` | ❌ |  |
| `cpuhotplug07.sh` | ❌ |  |
| `cpuhotplug_do_disk_write_loop` | ❌ |  |
| `cpuhotplug_do_kcompile_loop` | ❌ |  |
| `cpuhotplug_do_spin_loop` | ❌ |  |
| `cpuhotplug_hotplug.sh` | ❌ |  |
| `cpuhotplug_report_proc_interrupts` | ❌ |  |
| `cpuhotplug_testsuite.sh` | ❌ |  |
| `cpuset01` | ❌ |  |
| `crash01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `crash02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `creat04` | ❌ |  |
| `creat07` | ❌ |  |
| `creat07_child` | ❌ |  |
| `creat08` | ❌ |  |
| `creat09` | ❌ | 沙箱测不准:需 loop 块设备 |
| `create_datafile` | ❌ |  |
| `create_file` | ❌ |  |
| `crypto_user02` | ❌ |  |
| `cve-2015-3290` | ❌ |  |
| `daemonlib.sh` | ❌ |  |
| `data` | ❌ |  |
| `data_space` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `datafiles` | ❌ |  |
| `dccp01.sh` | ❌ |  |
| `dccp_ipsec.sh` | ❌ |  |
| `dccp_ipsec_vti.sh` | ❌ |  |
| `dctcp01.sh` | ❌ |  |
| `delete_module01` | ❌ |  |
| `delete_module02` | ❌ |  |
| `delete_module03` | ❌ |  |
| `df01.sh` | ❌ | 沙箱测不准:需 loop 块设备 |
| `dhcp_lib.sh` | ❌ |  |
| `dhcpd_tests.sh` | ❌ |  |
| `diotest1` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `diotest2` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `diotest3` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `diotest4` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `diotest5` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `diotest6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `dirty` | ❌ |  |
| `dirtyc0w` | ❌ |  |
| `dirtyc0w_child` | ❌ |  |
| `dirtyc0w_shmem` | ❌ |  |
| `dirtyc0w_shmem_child` | ❌ |  |
| `dma_thread_diotest` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `dns-stress-lib.sh` | ❌ |  |
| `dns-stress.sh` | ❌ |  |
| `dns-stress01-rmt.sh` | ❌ |  |
| `dns-stress02-rmt.sh` | ❌ |  |
| `dnsmasq_tests.sh` | ❌ |  |
| `doio` | ❌ |  |
| `dynamic_debug01.sh` | ❌ |  |
| `ebizzy` | ❌ |  |
| `eject-tests.sh` | ❌ |  |
| `eject_check_tray` | ❌ |  |
| `endian_switch01` | ❌ |  |
| `epoll-ltp` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `event_generator` | ❌ |  |
| `eventfd06` | ❌ |  |
| `evm_overlay.sh` | ❌ |  |
| `exec_with_inh` | ❌ |  |
| `exec_without_inh` | ❌ |  |
| `execl01_child` | ❌ |  |
| `execle01_child` | ❌ |  |
| `execlp01_child` | ❌ |  |
| `execv01_child` | ❌ |  |
| `execve01_child` | ❌ |  |
| `execve02` | ❌ |  |
| `execve03` | ❌ |  |
| `execve04` | ❌ |  |
| `execve06_child` | ❌ |  |
| `execve_child` | ❌ |  |
| `execveat03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `execveat_child` | ❌ |  |
| `execveat_errno` | ❌ |  |
| `execvp01_child` | ❌ |  |
| `exit01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `f00f` | ❌ |  |
| `fallocate01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fallocate02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fallocate04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fallocate05` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fallocate06` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify02` | ❌ |  |
| `fanotify03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify04` | ❌ |  |
| `fanotify05` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify06` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify07` | ❌ |  |
| `fanotify08` | ❌ |  |
| `fanotify09` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify10` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify11` | ❌ |  |
| `fanotify12` | ❌ |  |
| `fanotify13` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify14` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify15` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify16` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify17` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify18` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify19` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify20` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify21` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify22` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify23` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fanotify_child` | ❌ |  |
| `fchdir03` | ❌ |  |
| `fchmod02` | ❌ |  |
| `fchmod03` | ❌ |  |
| `fchmod05` | ❌ |  |
| `fchmod06` | ❌ |  |
| `fchown01_16` | ❌ |  |
| `fchown02_16` | ❌ |  |
| `fchown03` | ❌ |  |
| `fchown03_16` | ❌ |  |
| `fchown04` | ❌ |  |
| `fchown04_16` | ❌ |  |
| `fchown05_16` | ❌ |  |
| `fchownat01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fchownat02` | ❌ |  |
| `fcntl01` | ❌ |  |
| `fcntl01_64` | ❌ |  |
| `fcntl07` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl07_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl09` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl09_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl10` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl10_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl11` | ❌ |  |
| `fcntl11_64` | ❌ |  |
| `fcntl16` | ❌ |  |
| `fcntl16_64` | ❌ |  |
| `fcntl17` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl17_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl18` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl18_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl19` | ❌ |  |
| `fcntl19_64` | ❌ |  |
| `fcntl20` | ❌ |  |
| `fcntl20_64` | ❌ |  |
| `fcntl21` | ❌ |  |
| `fcntl21_64` | ❌ |  |
| `fcntl22` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl22_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl23` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl23_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl24` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl24_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl25` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl25_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl26` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl26_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl31` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl31_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl32` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl32_64` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fcntl33` | ❌ |  |
| `fcntl33_64` | ❌ |  |
| `fcntl35` | ❌ |  |
| `fcntl35_64` | ❌ |  |
| `fdatasync01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fdatasync02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fdatasync03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fgetxattr01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fgetxattr02` | ❌ |  |
| `filecapstest.sh` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `find_portbundle` | ❌ |  |
| `finit_module01` | ❌ |  |
| `flistxattr01` | ❌ |  |
| `flistxattr02` | ❌ |  |
| `flistxattr03` | ❌ |  |
| `float_bessel` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `float_exp_log` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `float_iperb` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `float_power` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `float_trigo` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `force_erase.sh` | ❌ |  |
| `fork05` | ❌ |  |
| `fork09` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fork13` | ❌ |  |
| `fork_exec_loop` | ❌ |  |
| `fork_freeze.sh` | ❌ |  |
| `fou01.sh` | ❌ |  |
| `fptest01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fptest02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `frag` | ❌ |  |
| `freeze_cancel.sh` | ❌ |  |
| `freeze_kill_thaw.sh` | ❌ |  |
| `freeze_move_thaw.sh` | ❌ |  |
| `freeze_self_thaw.sh` | ❌ |  |
| `freeze_sleep_thaw.sh` | ❌ |  |
| `freeze_thaw.sh` | ❌ |  |
| `freeze_write_freezing.sh` | ❌ |  |
| `fremovexattr01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fremovexattr02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fs_bind_lib.sh` | ❌ |  |
| `fs_di` | ❌ |  |
| `fs_fill` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fs_inod` | ❌ |  |
| `fs_perms` | ❌ |  |
| `fs_racer.sh` | ❌ |  |
| `fs_racer_dir_create.sh` | ❌ |  |
| `fs_racer_dir_test.sh` | ❌ |  |
| `fs_racer_file_concat.sh` | ❌ |  |
| `fs_racer_file_create.sh` | ❌ |  |
| `fs_racer_file_link.sh` | ❌ |  |
| `fs_racer_file_list.sh` | ❌ |  |
| `fs_racer_file_rename.sh` | ❌ |  |
| `fs_racer_file_rm.sh` | ❌ |  |
| `fs_racer_file_symlink.sh` | ❌ |  |
| `fsconfig01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsconfig02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsconfig03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsetxattr01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsetxattr02` | ❌ |  |
| `fsmount01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsmount02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsopen01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsopen02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fspick01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fspick02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsstress` | ❌ |  |
| `fstatat01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fstatfs01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fstatfs01_64` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsx.sh` | ❌ |  |
| `fsync01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `fsync04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `ftest01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ftest02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ftest03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ftest04` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ftest05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ftest06` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ftest07` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ftest08` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ftp-download-stress.sh` | ❌ |  |
| `ftp-download-stress01-rmt.sh` | ❌ |  |
| `ftp-download-stress02-rmt.sh` | ❌ |  |
| `ftp-upload-stress.sh` | ❌ |  |
| `ftp-upload-stress01-rmt.sh` | ❌ |  |
| `ftp-upload-stress02-rmt.sh` | ❌ |  |
| `ftp01.sh` | ❌ |  |
| `ftrace_lib.sh` | ❌ |  |
| `ftrace_regression01.sh` | ❌ |  |
| `ftrace_regression02.sh` | ❌ |  |
| `ftrace_stress` | ❌ |  |
| `ftrace_stress_test.sh` | ❌ |  |
| `ftruncate04` | ❌ |  |
| `ftruncate04_64` | ❌ |  |
| `futex_wake04` | ❌ |  |
| `futimesat01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `fw_load` | ❌ |  |
| `genacos` | ❌ |  |
| `genasin` | ❌ |  |
| `genatan` | ❌ |  |
| `genatan2` | ❌ |  |
| `genbessel` | ❌ |  |
| `genceil` | ❌ |  |
| `gencos` | ❌ |  |
| `gencosh` | ❌ |  |
| `geneve01.sh` | ❌ |  |
| `geneve02.sh` | ❌ |  |
| `genexp` | ❌ |  |
| `genexp_log` | ❌ |  |
| `genfabs` | ❌ |  |
| `genfloor` | ❌ |  |
| `genfmod` | ❌ |  |
| `genfrexp` | ❌ |  |
| `genhypot` | ❌ |  |
| `geniperb` | ❌ |  |
| `genj0` | ❌ |  |
| `genj1` | ❌ |  |
| `genldexp` | ❌ |  |
| `genlgamma` | ❌ |  |
| `genload` | ❌ |  |
| `genlog` | ❌ |  |
| `genlog10` | ❌ |  |
| `genmodf` | ❌ |  |
| `genpow` | ❌ |  |
| `genpower` | ❌ |  |
| `gensin` | ❌ |  |
| `gensinh` | ❌ |  |
| `gensqrt` | ❌ |  |
| `gentan` | ❌ |  |
| `gentanh` | ❌ |  |
| `gentrigo` | ❌ |  |
| `geny0` | ❌ |  |
| `geny1` | ❌ |  |
| `get_ifname` | ❌ |  |
| `get_mempolicy01` | ❌ |  |
| `get_mempolicy02` | ❌ |  |
| `get_robust_list01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `getdents01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `getdents02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `geteuid01_16` | ❌ |  |
| `geteuid02_16` | ❌ |  |
| `getgid01` | ❌ |  |
| `getgid01_16` | ❌ |  |
| `getgid03_16` | ❌ |  |
| `getgroups01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `getgroups01_16` | ❌ |  |
| `getgroups03` | ❌ |  |
| `getgroups03_16` | ❌ |  |
| `getrandom05` | ❌ |  |
| `getresgid01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `getresgid01_16` | ❌ |  |
| `getresgid02` | ❌ |  |
| `getresgid02_16` | ❌ |  |
| `getresgid03` | ❌ |  |
| `getresgid03_16` | ❌ |  |
| `getresuid01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `getresuid01_16` | ❌ |  |
| `getresuid02` | ❌ |  |
| `getresuid02_16` | ❌ |  |
| `getresuid03` | ❌ |  |
| `getresuid03_16` | ❌ |  |
| `getrusage03_child` | ❌ |  |
| `getrusage04` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `getuid01_16` | ❌ |  |
| `getuid03_16` | ❌ |  |
| `getxattr02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `getxattr03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `getxattr04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `getxattr05` | ❌ |  |
| `gre01.sh` | ❌ |  |
| `gre02.sh` | ❌ |  |
| `growfiles` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `hackbench` | ❌ |  |
| `hangup01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ht_affinity` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ht_enabled` | ❌ |  |
| `http-stress.sh` | ❌ |  |
| `http-stress01-rmt.sh` | ❌ |  |
| `http-stress02-rmt.sh` | ❌ |  |
| `hugefallocate01` | ❌ |  |
| `hugefallocate02` | ❌ |  |
| `hugefork01` | ❌ |  |
| `hugefork02` | ❌ |  |
| `hugemmap01` | ❌ |  |
| `hugemmap02` | ❌ |  |
| `hugemmap04` | ❌ |  |
| `hugemmap05` | ❌ |  |
| `hugemmap06` | ❌ |  |
| `hugemmap07` | ❌ |  |
| `hugemmap08` | ❌ |  |
| `hugemmap09` | ❌ |  |
| `hugemmap10` | ❌ |  |
| `hugemmap11` | ❌ |  |
| `hugemmap12` | ❌ |  |
| `hugemmap13` | ❌ |  |
| `hugemmap14` | ❌ |  |
| `hugemmap15` | ❌ |  |
| `hugemmap16` | ❌ |  |
| `hugemmap17` | ❌ |  |
| `hugemmap18` | ❌ |  |
| `hugemmap19` | ❌ |  |
| `hugemmap20` | ❌ |  |
| `hugemmap21` | ❌ |  |
| `hugemmap22` | ❌ |  |
| `hugemmap23` | ❌ |  |
| `hugemmap24` | ❌ |  |
| `hugemmap25` | ❌ |  |
| `hugemmap26` | ❌ |  |
| `hugemmap27` | ❌ |  |
| `hugemmap28` | ❌ |  |
| `hugemmap29` | ❌ |  |
| `hugemmap30` | ❌ |  |
| `hugemmap31` | ❌ |  |
| `hugemmap32` | ❌ |  |
| `hugeshmat01` | ❌ |  |
| `hugeshmat02` | ❌ |  |
| `hugeshmat03` | ❌ |  |
| `hugeshmat04` | ❌ |  |
| `hugeshmat05` | ❌ |  |
| `hugeshmctl01` | ❌ |  |
| `hugeshmctl02` | ❌ |  |
| `hugeshmctl03` | ❌ |  |
| `hugeshmdt01` | ❌ |  |
| `hugeshmget01` | ❌ |  |
| `hugeshmget02` | ❌ |  |
| `hugeshmget03` | ❌ |  |
| `hugeshmget05` | ❌ |  |
| `icmp-uni-basic.sh` | ❌ |  |
| `icmp-uni-vti.sh` | ❌ |  |
| `icmp4-multi-diffip01` | ❌ |  |
| `icmp4-multi-diffip02` | ❌ |  |
| `icmp4-multi-diffip03` | ❌ |  |
| `icmp4-multi-diffip04` | ❌ |  |
| `icmp4-multi-diffip05` | ❌ |  |
| `icmp4-multi-diffip06` | ❌ |  |
| `icmp4-multi-diffip07` | ❌ |  |
| `icmp4-multi-diffnic01` | ❌ |  |
| `icmp4-multi-diffnic02` | ❌ |  |
| `icmp4-multi-diffnic03` | ❌ |  |
| `icmp4-multi-diffnic04` | ❌ |  |
| `icmp4-multi-diffnic05` | ❌ |  |
| `icmp4-multi-diffnic06` | ❌ |  |
| `icmp4-multi-diffnic07` | ❌ |  |
| `icmp6-multi-diffip01` | ❌ |  |
| `icmp6-multi-diffip02` | ❌ |  |
| `icmp6-multi-diffip03` | ❌ |  |
| `icmp6-multi-diffip04` | ❌ |  |
| `icmp6-multi-diffip05` | ❌ |  |
| `icmp6-multi-diffip06` | ❌ |  |
| `icmp6-multi-diffip07` | ❌ |  |
| `icmp6-multi-diffnic01` | ❌ |  |
| `icmp6-multi-diffnic02` | ❌ |  |
| `icmp6-multi-diffnic03` | ❌ |  |
| `icmp6-multi-diffnic04` | ❌ |  |
| `icmp6-multi-diffnic05` | ❌ |  |
| `icmp6-multi-diffnic06` | ❌ |  |
| `icmp6-multi-diffnic07` | ❌ |  |
| `if-lib.sh` | ❌ |  |
| `ima_boot_aggregate` | ❌ |  |
| `ima_conditionals.sh` | ❌ |  |
| `ima_kexec.sh` | ❌ |  |
| `ima_keys.sh` | ❌ |  |
| `ima_measurements.sh` | ❌ |  |
| `ima_mmap` | ❌ |  |
| `ima_selinux.sh` | ❌ |  |
| `ima_setup.sh` | ❌ |  |
| `ima_tpm.sh` | ❌ |  |
| `ima_violations.sh` | ❌ |  |
| `inh_capped` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `init_module01` | ❌ |  |
| `init_module02` | ❌ |  |
| `initialize_if` | ❌ |  |
| `inode01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `inode02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `inotify03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `inotify06` | ❌ |  |
| `inotify07` | ❌ | 沙箱测不准:需 loop 块设备 |
| `inotify08` | ❌ | 沙箱测不准:需 loop 块设备 |
| `input01` | ❌ |  |
| `input02` | ❌ |  |
| `input03` | ❌ |  |
| `input04` | ❌ |  |
| `input05` | ❌ |  |
| `input06` | ❌ |  |
| `insmod01.sh` | ❌ |  |
| `io_cancel02` | ❌ |  |
| `io_control01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `io_destroy01` | ❌ |  |
| `io_getevents02` | ❌ |  |
| `io_pgetevents01` | ❌ |  |
| `io_pgetevents02` | ❌ |  |
| `io_setup01` | ❌ |  |
| `io_submit01` | ❌ |  |
| `io_uring01` | ❌ | 沙箱测不准:需写全局 sysctl |
| `io_uring02` | ❌ | 沙箱测不准:需写全局 sysctl |
| `ioctl02` | ❌ |  |
| `ioctl04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `ioctl05` | ❌ | 沙箱测不准:需 loop 块设备 |
| `ioctl06` | ❌ | 沙箱测不准:需 loop 块设备 |
| `ioctl08` | ❌ | 沙箱测不准:需 loop 块设备 |
| `ioctl09` | ❌ |  |
| `ioctl_loop01` | ❌ |  |
| `ioctl_loop02` | ❌ |  |
| `ioctl_loop03` | ❌ |  |
| `ioctl_loop04` | ❌ |  |
| `ioctl_loop05` | ❌ |  |
| `ioctl_loop06` | ❌ |  |
| `ioctl_loop07` | ❌ |  |
| `ioctl_sg01` | ❌ |  |
| `iogen` | ❌ |  |
| `ioperm01` | ❌ |  |
| `ioperm02` | ❌ |  |
| `iopl02` | ❌ |  |
| `ipneigh01.sh` | ❌ |  |
| `ipsec_lib.sh` | ❌ |  |
| `iptables_lib.sh` | ❌ |  |
| `irqbalance01` | ❌ |  |
| `isofs.sh` | ❌ |  |
| `kernbench` | ❌ |  |
| `keyctl01.sh` | ❌ |  |
| `keyctl02` | ❌ |  |
| `kill02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `kill05` | ❌ |  |
| `kill07` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `kill08` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `kill09` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `kill10` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `kill12` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `kill13` | ❌ |  |
| `killall_icmp_traffic` | ❌ |  |
| `killall_tcp_traffic` | ❌ |  |
| `killall_udp_traffic` | ❌ |  |
| `kmsg01` | ❌ |  |
| `ksm01` | ❌ |  |
| `ksm02` | ❌ |  |
| `ksm03` | ❌ |  |
| `ksm04` | ❌ |  |
| `ksm05` | ❌ |  |
| `ksm06` | ❌ |  |
| `ksm07` | ❌ |  |
| `lchown01` | ❌ |  |
| `lchown01_16` | ❌ |  |
| `lchown02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `lchown02_16` | ❌ |  |
| `lchown03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `lchown03_16` | ❌ | 沙箱测不准:需 loop 块设备 |
| `leapsec01` | ❌ |  |
| `lgetxattr01` | ❌ |  |
| `lgetxattr02` | ❌ |  |
| `libcgroup_freezer` | ❌ |  |
| `linkat01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `linkat02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `listen01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `listxattr01` | ❌ |  |
| `listxattr02` | ❌ |  |
| `listxattr03` | ❌ |  |
| `llistxattr01` | ❌ |  |
| `llistxattr02` | ❌ |  |
| `llistxattr03` | ❌ |  |
| `lock_torture.sh` | ❌ |  |
| `locktests` | ❌ |  |
| `logrotate_tests.sh` | ❌ |  |
| `lremovexattr01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `lstat02` | ❌ |  |
| `lstat02_64` | ❌ |  |
| `ltpClient` | ❌ |  |
| `ltpServer` | ❌ |  |
| `ltpSockets.sh` | ❌ |  |
| `ltp_acpi` | ❌ |  |
| `macsec01.sh` | ❌ |  |
| `macsec02.sh` | ❌ |  |
| `macsec03.sh` | ❌ |  |
| `macsec_lib.sh` | ❌ |  |
| `madvise06` | ❌ | 沙箱测不准:需写全局 sysctl |
| `madvise07` | ❌ |  |
| `madvise08` | ❌ | 沙箱测不准:需写全局 sysctl |
| `madvise09` | ❌ |  |
| `madvise11` | ❌ |  |
| `max_map_count` | ❌ | 沙箱测不准:需写全局 sysctl |
| `mbind01` | ❌ |  |
| `mbind02` | ❌ |  |
| `mbind03` | ❌ |  |
| `mbind04` | ❌ |  |
| `mc_cmds.sh` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mc_commo.sh` | ❌ |  |
| `mc_member.sh` | ❌ |  |
| `mc_member_test` | ❌ |  |
| `mc_opts.sh` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mc_recv` | ❌ |  |
| `mc_send` | ❌ |  |
| `mc_verify_opts` | ❌ |  |
| `mc_verify_opts_error` | ❌ |  |
| `mcast-lib.sh` | ❌ |  |
| `meltdown` | ❌ |  |
| `mem02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mem_process` | ❌ |  |
| `memcg_control_test.sh` | ❌ |  |
| `memcg_failcnt.sh` | ❌ |  |
| `memcg_force_empty.sh` | ❌ |  |
| `memcg_lib.sh` | ❌ |  |
| `memcg_limit_in_bytes.sh` | ❌ |  |
| `memcg_max_usage_in_bytes_test.sh` | ❌ |  |
| `memcg_memsw_limit_in_bytes_test.sh` | ❌ |  |
| `memcg_move_charge_at_immigrate_test.sh` | ❌ |  |
| `memcg_process` | ❌ |  |
| `memcg_process_stress` | ❌ |  |
| `memcg_regression_test.sh` | ❌ |  |
| `memcg_stat_rss.sh` | ❌ |  |
| `memcg_stat_test.sh` | ❌ |  |
| `memcg_stress_test.sh` | ❌ |  |
| `memcg_subgroup_charge.sh` | ❌ |  |
| `memcg_test_1` | ❌ |  |
| `memcg_test_2` | ❌ |  |
| `memcg_test_3` | ❌ |  |
| `memcg_test_4` | ❌ |  |
| `memcg_test_4.sh` | ❌ |  |
| `memcg_usage_in_bytes_test.sh` | ❌ |  |
| `memcg_use_hierarchy_test.sh` | ❌ |  |
| `memcontrol01` | ❌ |  |
| `memcontrol02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `memcontrol03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `memcontrol04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `memctl_test01` | ❌ |  |
| `memfd_create03` | ❌ |  |
| `memtoy` | ❌ |  |
| `migrate_pages01` | ❌ |  |
| `migrate_pages02` | ❌ |  |
| `migrate_pages03` | ❌ |  |
| `min_free_kbytes` | ❌ | 沙箱测不准:需写全局 sysctl |
| `mincore01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mkdir02` | ❌ |  |
| `mkdir04` | ❌ |  |
| `mkdir05` | ❌ |  |
| `mkdir09` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mkdirat01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mkfs01.sh` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mknod02` | ❌ |  |
| `mknod03` | ❌ |  |
| `mknod04` | ❌ |  |
| `mknod05` | ❌ |  |
| `mknod06` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mknod07` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mknod08` | ❌ |  |
| `mknodat01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mknodat02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mkswap01.sh` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mlockall01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mlockall02` | ❌ |  |
| `mlockall03` | ❌ |  |
| `mmap-corruption01` | ❌ |  |
| `mmap001` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmap01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmap03` | ❌ |  |
| `mmap10` | ❌ |  |
| `mmap11` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmap14` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmap16` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mmap2` | ❌ |  |
| `mmapstress02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmapstress03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmapstress05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmapstress06` | ❌ |  |
| `mmapstress07` | ❌ |  |
| `mmapstress08` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmapstress09` | ❌ |  |
| `mmapstress10` | ❌ |  |
| `mmstress` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mmstress_dummy` | ❌ |  |
| `modify_ldt01` | ❌ |  |
| `modify_ldt02` | ❌ |  |
| `modify_ldt03` | ❌ |  |
| `mount01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mount02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mount03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mount03_suid_child` | ❌ |  |
| `mount04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mount05` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mount06` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mount07` | ❌ | 沙箱测不准:需 loop 块设备 |
| `mount_setattr01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `move_mount01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `move_mount02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `move_pages01` | ❌ |  |
| `move_pages02` | ❌ |  |
| `move_pages03` | ❌ |  |
| `move_pages04` | ❌ |  |
| `move_pages05` | ❌ |  |
| `move_pages06` | ❌ |  |
| `move_pages07` | ❌ |  |
| `move_pages09` | ❌ |  |
| `move_pages10` | ❌ |  |
| `move_pages11` | ❌ |  |
| `move_pages12` | ❌ |  |
| `mpls01.sh` | ❌ |  |
| `mpls02.sh` | ❌ |  |
| `mpls03.sh` | ❌ |  |
| `mpls04.sh` | ❌ |  |
| `mpls_lib.sh` | ❌ |  |
| `mprotect01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mprotect02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mprotect03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mprotect04` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mqns_03` | ❌ |  |
| `mqns_04` | ❌ |  |
| `mremap01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mremap02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mremap03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mremap04` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `mremap05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `msgctl04` | ❌ |  |
| `msgctl05` | ❌ |  |
| `msgctl06` | ❌ |  |
| `msgget03` | ❌ | 沙箱测不准:需写全局 sysctl |
| `msgget04` | ❌ |  |
| `msgget05` | ❌ |  |
| `msgstress01` | ❌ |  |
| `msync01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `msync02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `msync03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `msync04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `munmap01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `munmap02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `munmap03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `myfunctions.sh` | ❌ |  |
| `name_to_handle_at01` | ❌ |  |
| `net_cmdlib.sh` | ❌ |  |
| `netns_lib.sh` | ❌ |  |
| `netstress` | ❌ |  |
| `newuname01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `nextafter01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `nfs01.sh` | ❌ |  |
| `nfs01_open_files` | ❌ |  |
| `nfs02.sh` | ❌ |  |
| `nfs03.sh` | ❌ |  |
| `nfs04.sh` | ❌ |  |
| `nfs04_create_file` | ❌ |  |
| `nfs05.sh` | ❌ |  |
| `nfs05_make_tree` | ❌ |  |
| `nfs06.sh` | ❌ |  |
| `nfs07.sh` | ❌ |  |
| `nfs08.sh` | ❌ |  |
| `nfs09.sh` | ❌ |  |
| `nfs_flock` | ❌ |  |
| `nfs_flock_dgen` | ❌ |  |
| `nfs_lib.sh` | ❌ |  |
| `nfslock01.sh` | ❌ |  |
| `nfsstat01.sh` | ❌ |  |
| `nftw01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `nftw6401` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `nice01` | ❌ |  |
| `nice04` | ❌ |  |
| `nice05` | ❌ |  |
| `nptl01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ns-echoclient` | ❌ |  |
| `ns-icmp_redirector` | ❌ |  |
| `ns-icmpv4_sender` | ❌ |  |
| `ns-icmpv6_sender` | ❌ |  |
| `ns-igmp_querier` | ❌ |  |
| `ns-mcast_join` | ❌ |  |
| `ns-mcast_receiver` | ❌ |  |
| `ns-tcpclient` | ❌ |  |
| `ns-tcpserver` | ❌ |  |
| `ns-udpclient` | ❌ |  |
| `ns-udpsender` | ❌ |  |
| `ns-udpserver` | ❌ |  |
| `numa01.sh` | ❌ |  |
| `oom01` | ❌ | 沙箱测不准:需写全局 sysctl |
| `oom02` | ❌ |  |
| `oom03` | ❌ |  |
| `oom04` | ❌ |  |
| `oom05` | ❌ |  |
| `open02` | ❌ |  |
| `open08` | ❌ |  |
| `open10` | ❌ |  |
| `open11` | ❌ |  |
| `open12` | ❌ | 沙箱测不准:需 loop 块设备 |
| `open12_child` | ❌ |  |
| `open13` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `open14` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `open_by_handle_at01` | ❌ |  |
| `open_tree01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `open_tree02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `openat02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `openat02_child` | ❌ |  |
| `openat03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `openat04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `openfile` | ❌ |  |
| `output_ipsec_conf` | ❌ |  |
| `overcommit_memory` | ❌ | 沙箱测不准:需写全局 sysctl |
| `page01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `page02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `parameters.sh` | ❌ |  |
| `pause02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pause03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pcrypt_aead01` | ❌ |  |
| `pec_listener` | ❌ |  |
| `perf_event_open01` | ❌ |  |
| `perf_event_open02` | ❌ |  |
| `perf_event_open03` | ❌ |  |
| `pidfd_send_signal02` | ❌ |  |
| `pids.sh` | ❌ |  |
| `pids_task1` | ❌ |  |
| `pids_task2` | ❌ |  |
| `pipe04` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pipe05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pipe09` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pipe2_02_child` | ❌ |  |
| `pipeio` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pkey01` | ❌ |  |
| `pm_cpu_consolidation.py` | ❌ |  |
| `pm_get_sched_values` | ❌ |  |
| `pm_ilb_test.py` | ❌ |  |
| `pm_include.sh` | ❌ |  |
| `pm_sched_domain.py` | ❌ |  |
| `pm_sched_mc.py` | ❌ |  |
| `prctl06` | ❌ | 沙箱测不准:需 loop 块设备 |
| `prctl06_execve` | ❌ |  |
| `preadv03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `preadv03_64` | ❌ | 沙箱测不准:需 loop 块设备 |
| `preadv203` | ❌ | 沙箱测不准:需 loop 块设备 |
| `preadv203_64` | ❌ | 沙箱测不准:需 loop 块设备 |
| `prepare_lvm.sh` | ❌ |  |
| `print_caps` | ❌ |  |
| `proc01` | ❌ |  |
| `proc_sched_rt01` | ❌ |  |
| `process_madvise01` | ❌ |  |
| `profil01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `prot_hsymlinks` | ❌ |  |
| `pt_test` | ❌ |  |
| `ptem01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pth_str01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pth_str02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pth_str03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pthcli` | ❌ |  |
| `pthserv` | ❌ |  |
| `ptrace02` | ❌ |  |
| `ptrace04` | ❌ |  |
| `ptrace05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `ptrace06` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `pty01` | ❌ |  |
| `pty03` | ❌ |  |
| `pty04` | ❌ |  |
| `pty06` | ❌ |  |
| `pty07` | ❌ |  |
| `pwritev03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `pwritev03_64` | ❌ | 沙箱测不准:需 loop 块设备 |
| `quota_remount_test01.sh` | ❌ |  |
| `quotactl01` | ❌ |  |
| `quotactl02` | ❌ |  |
| `quotactl03` | ❌ |  |
| `quotactl04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `quotactl05` | ❌ |  |
| `quotactl06` | ❌ |  |
| `quotactl07` | ❌ |  |
| `quotactl08` | ❌ | 沙箱测不准:需 loop 块设备 |
| `quotactl09` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rcu_torture.sh` | ❌ |  |
| `read_all` | ❌ |  |
| `readahead02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `readdir01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `readdir21` | ❌ | 沙箱测不准:需 loop 块设备 |
| `readlink03` | ❌ |  |
| `reboot01` | ❌ |  |
| `recv01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `recvfrom01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `recvmsg03` | ❌ |  |
| `remap_file_pages01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `remove_password.sh` | ❌ |  |
| `removexattr01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `removexattr02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `rename01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename05` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename06` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename07` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename08` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename09` | ❌ |  |
| `rename10` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename11` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename12` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename13` | ❌ | 沙箱测不准:需 loop 块设备 |
| `rename14` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `renameat01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `renameat201` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `renameat202` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `rmdir03` | ❌ |  |
| `route-change-netlink` | ❌ |  |
| `route-lib.sh` | ❌ |  |
| `route4-rmmod` | ❌ |  |
| `route6-rmmod` | ❌ |  |
| `rt_sigaction01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `rt_sigaction02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `rt_sigaction03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `rt_sigprocmask01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `rt_sigprocmask02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `rtc01` | ❌ |  |
| `rtc02` | ❌ |  |
| `run_capbounds.sh` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `run_cpuctl_latency_test.sh` | ❌ |  |
| `run_cpuctl_stress_test.sh` | ❌ |  |
| `run_cpuctl_test.sh` | ❌ |  |
| `run_cpuctl_test_fj.sh` | ❌ |  |
| `run_freezer.sh` | ❌ |  |
| `run_memctl_test.sh` | ❌ |  |
| `run_sched_cliserv.sh` | ❌ |  |
| `runpwtests01.sh` | ❌ |  |
| `runpwtests02.sh` | ❌ |  |
| `runpwtests03.sh` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `runpwtests04.sh` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `runpwtests05.sh` | ❌ |  |
| `runpwtests06.sh` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `runpwtests_exclusive01.sh` | ❌ |  |
| `runpwtests_exclusive02.sh` | ❌ |  |
| `runpwtests_exclusive03.sh` | ❌ |  |
| `runpwtests_exclusive04.sh` | ❌ |  |
| `runpwtests_exclusive05.sh` | ❌ |  |
| `rwtest` | ❌ |  |
| `sbrk03` | ❌ |  |
| `sched_datafile` | ❌ |  |
| `sched_driver` | ❌ |  |
| `sched_getattr01` | ❌ |  |
| `sched_getattr02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sched_rr_get_interval02` | ❌ |  |
| `sched_setattr01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sched_setparam03` | ❌ |  |
| `sched_setparam05` | ❌ |  |
| `sched_setscheduler02` | ❌ |  |
| `sched_setscheduler03` | ❌ |  |
| `sched_stress.sh` | ❌ |  |
| `sched_tc0` | ❌ |  |
| `sched_tc1` | ❌ |  |
| `sched_tc2` | ❌ |  |
| `sched_tc3` | ❌ |  |
| `sched_tc4` | ❌ |  |
| `sched_tc5` | ❌ |  |
| `sched_tc6` | ❌ |  |
| `sched_yield01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sctp01.sh` | ❌ |  |
| `sctp_ipsec.sh` | ❌ |  |
| `sctp_ipsec_vti.sh` | ❌ |  |
| `semctl02` | ❌ |  |
| `semctl04` | ❌ |  |
| `semctl06` | ❌ |  |
| `semctl08` | ❌ |  |
| `semctl09` | ❌ |  |
| `semget05` | ❌ | 沙箱测不准:需写全局 sysctl |
| `semop01` | ❌ |  |
| `semop02` | ❌ |  |
| `semop05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `send01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sendmsg01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sendmsg02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sendto01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `set_ipv4addr` | ❌ |  |
| `set_mempolicy01` | ❌ |  |
| `set_mempolicy02` | ❌ |  |
| `set_mempolicy03` | ❌ |  |
| `set_mempolicy04` | ❌ |  |
| `set_mempolicy05` | ❌ |  |
| `set_robust_list01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `set_thread_area01` | ❌ |  |
| `set_tid_address01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `setdomainname01` | ❌ |  |
| `setdomainname02` | ❌ |  |
| `setdomainname03` | ❌ |  |
| `setegid01` | ❌ |  |
| `setegid02` | ❌ |  |
| `setfsgid01_16` | ❌ |  |
| `setfsgid02` | ❌ |  |
| `setfsgid02_16` | ❌ |  |
| `setfsgid03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `setfsgid03_16` | ❌ |  |
| `setfsuid01_16` | ❌ |  |
| `setfsuid02_16` | ❌ |  |
| `setfsuid03` | ❌ |  |
| `setfsuid03_16` | ❌ |  |
| `setfsuid04` | ❌ |  |
| `setfsuid04_16` | ❌ |  |
| `setgid01_16` | ❌ |  |
| `setgid02` | ❌ |  |
| `setgid02_16` | ❌ |  |
| `setgid03` | ❌ |  |
| `setgid03_16` | ❌ |  |
| `setgroups01` | ❌ |  |
| `setgroups01_16` | ❌ |  |
| `setgroups02` | ❌ |  |
| `setgroups02_16` | ❌ |  |
| `setgroups03` | ❌ |  |
| `setgroups03_16` | ❌ |  |
| `setgroups04` | ❌ |  |
| `setgroups04_16` | ❌ |  |
| `sethostname01` | ❌ |  |
| `sethostname02` | ❌ |  |
| `sethostname03` | ❌ |  |
| `setpgid01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `setpgid03_child` | ❌ |  |
| `setpgrp01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `setpriority01` | ❌ |  |
| `setregid01_16` | ❌ |  |
| `setregid02` | ❌ |  |
| `setregid02_16` | ❌ |  |
| `setregid03` | ❌ |  |
| `setregid03_16` | ❌ |  |
| `setregid04_16` | ❌ |  |
| `setresgid01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `setresgid01_16` | ❌ |  |
| `setresgid02` | ❌ |  |
| `setresgid02_16` | ❌ |  |
| `setresgid03` | ❌ |  |
| `setresgid03_16` | ❌ |  |
| `setresgid04` | ❌ |  |
| `setresgid04_16` | ❌ |  |
| `setresuid01_16` | ❌ |  |
| `setresuid02` | ❌ |  |
| `setresuid02_16` | ❌ |  |
| `setresuid03` | ❌ |  |
| `setresuid03_16` | ❌ |  |
| `setresuid04_16` | ❌ |  |
| `setresuid05` | ❌ |  |
| `setresuid05_16` | ❌ |  |
| `setreuid01_16` | ❌ |  |
| `setreuid02_16` | ❌ |  |
| `setreuid03` | ❌ |  |
| `setreuid03_16` | ❌ |  |
| `setreuid04` | ❌ |  |
| `setreuid04_16` | ❌ |  |
| `setreuid05_16` | ❌ |  |
| `setreuid06` | ❌ |  |
| `setreuid06_16` | ❌ |  |
| `setreuid07_16` | ❌ |  |
| `setrlimit01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `setrlimit02` | ❌ |  |
| `setsid01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `setsockopt04` | ❌ |  |
| `setsockopt10` | ❌ |  |
| `settimeofday01` | ❌ |  |
| `setuid01_16` | ❌ |  |
| `setuid03` | ❌ |  |
| `setuid03_16` | ❌ |  |
| `setuid04` | ❌ |  |
| `setuid04_16` | ❌ |  |
| `setxattr01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `setxattr02` | ❌ |  |
| `setxattr03` | ❌ |  |
| `sgetmask01` | ❌ |  |
| `shell_pipe01.sh` | ❌ |  |
| `shm_test` | ❌ |  |
| `shmat1` | ❌ |  |
| `shmctl02` | ❌ |  |
| `shmctl04` | ❌ |  |
| `shmctl06` | ❌ |  |
| `shmget02` | ❌ | 沙箱测不准:需写全局 sysctl |
| `shmget04` | ❌ |  |
| `shmget05` | ❌ |  |
| `shmget06` | ❌ |  |
| `shmt02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `shmt03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `shmt04` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `shmt05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `shmt06` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `shmt07` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `shmt08` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `shmt09` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `shmt10` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sigaction01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sigaction02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sigaltstack01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `signal06` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `signalfd4_01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `signalfd4_02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sigprocmask01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sigrelse01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sit01.sh` | ❌ |  |
| `smack_common.sh` | ❌ |  |
| `smack_file_access.sh` | ❌ |  |
| `smack_notroot` | ❌ |  |
| `smack_set_ambient.sh` | ❌ |  |
| `smack_set_cipso.sh` | ❌ |  |
| `smack_set_current.sh` | ❌ |  |
| `smack_set_direct.sh` | ❌ |  |
| `smack_set_doi.sh` | ❌ |  |
| `smack_set_load.sh` | ❌ |  |
| `smack_set_netlabel.sh` | ❌ |  |
| `smack_set_onlycap.sh` | ❌ |  |
| `smack_set_socket_labels` | ❌ |  |
| `smt_smp_affinity.sh` | ❌ |  |
| `smt_smp_enabled.sh` | ❌ |  |
| `socketcall01` | ❌ |  |
| `socketcall02` | ❌ |  |
| `socketcall03` | ❌ |  |
| `sockioctl01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `splice06` | ❌ | 沙箱测不准:需写全局 sysctl |
| `squashfs01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `ssetmask01` | ❌ |  |
| `ssh-stress.sh` | ❌ |  |
| `stack_space` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `stat01` | ❌ |  |
| `stat01_64` | ❌ |  |
| `stat03` | ❌ |  |
| `stat03_64` | ❌ |  |
| `statfs01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statfs01_64` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statfs03` | ❌ |  |
| `statfs03_64` | ❌ |  |
| `statvfs01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statx01` | ❌ |  |
| `statx04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statx05` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statx06` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statx07` | ❌ |  |
| `statx08` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statx09` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statx10` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statx11` | ❌ | 沙箱测不准:需 loop 块设备 |
| `statx12` | ❌ | 沙箱测不准:需 loop 块设备 |
| `stime01` | ❌ |  |
| `stime02` | ❌ |  |
| `stop_freeze_sleep_thaw_cont.sh` | ❌ |  |
| `stop_freeze_thaw_cont.sh` | ❌ |  |
| `stream01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `stream02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `stream03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `stream04` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `stream05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `stress` | ❌ |  |
| `string01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `support_numa` | ❌ |  |
| `swapoff01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `swapoff02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `swapon01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `swapon02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `swapon03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `swapping01` | ❌ |  |
| `symlink01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `symlink03` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `symlinkat01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sync01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `sync_file_range02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `syncfs01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `sysconf01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sysctl01` | ❌ |  |
| `sysctl01.sh` | ❌ |  |
| `sysctl03` | ❌ |  |
| `sysctl04` | ❌ |  |
| `sysinfo01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `sysinfo02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `syslog11` | ❌ | 沙箱测不准:需写全局 sysctl |
| `syslog12` | ❌ |  |
| `tbio` | ❌ |  |
| `tc01.sh` | ❌ |  |
| `tcindex01` | ❌ |  |
| `tcp4-multi-diffip01` | ❌ |  |
| `tcp4-multi-diffip02` | ❌ |  |
| `tcp4-multi-diffip03` | ❌ |  |
| `tcp4-multi-diffip04` | ❌ |  |
| `tcp4-multi-diffip05` | ❌ |  |
| `tcp4-multi-diffip06` | ❌ |  |
| `tcp4-multi-diffip07` | ❌ |  |
| `tcp4-multi-diffip08` | ❌ |  |
| `tcp4-multi-diffip09` | ❌ |  |
| `tcp4-multi-diffip10` | ❌ |  |
| `tcp4-multi-diffip11` | ❌ |  |
| `tcp4-multi-diffip12` | ❌ |  |
| `tcp4-multi-diffip13` | ❌ |  |
| `tcp4-multi-diffip14` | ❌ |  |
| `tcp4-multi-diffnic01` | ❌ |  |
| `tcp4-multi-diffnic02` | ❌ |  |
| `tcp4-multi-diffnic03` | ❌ |  |
| `tcp4-multi-diffnic04` | ❌ |  |
| `tcp4-multi-diffnic05` | ❌ |  |
| `tcp4-multi-diffnic06` | ❌ |  |
| `tcp4-multi-diffnic07` | ❌ |  |
| `tcp4-multi-diffnic08` | ❌ |  |
| `tcp4-multi-diffnic09` | ❌ |  |
| `tcp4-multi-diffnic10` | ❌ |  |
| `tcp4-multi-diffnic11` | ❌ |  |
| `tcp4-multi-diffnic12` | ❌ |  |
| `tcp4-multi-diffnic13` | ❌ |  |
| `tcp4-multi-diffnic14` | ❌ |  |
| `tcp4-multi-diffport01` | ❌ |  |
| `tcp4-multi-diffport02` | ❌ |  |
| `tcp4-multi-diffport03` | ❌ |  |
| `tcp4-multi-diffport04` | ❌ |  |
| `tcp4-multi-diffport05` | ❌ |  |
| `tcp4-multi-diffport06` | ❌ |  |
| `tcp4-multi-diffport07` | ❌ |  |
| `tcp4-multi-diffport08` | ❌ |  |
| `tcp4-multi-diffport09` | ❌ |  |
| `tcp4-multi-diffport10` | ❌ |  |
| `tcp4-multi-diffport11` | ❌ |  |
| `tcp4-multi-diffport12` | ❌ |  |
| `tcp4-multi-diffport13` | ❌ |  |
| `tcp4-multi-diffport14` | ❌ |  |
| `tcp4-multi-sameport01` | ❌ |  |
| `tcp4-multi-sameport02` | ❌ |  |
| `tcp4-multi-sameport03` | ❌ |  |
| `tcp4-multi-sameport04` | ❌ |  |
| `tcp4-multi-sameport05` | ❌ |  |
| `tcp4-multi-sameport06` | ❌ |  |
| `tcp4-multi-sameport07` | ❌ |  |
| `tcp4-multi-sameport08` | ❌ |  |
| `tcp4-multi-sameport09` | ❌ |  |
| `tcp4-multi-sameport10` | ❌ |  |
| `tcp4-multi-sameport11` | ❌ |  |
| `tcp4-multi-sameport12` | ❌ |  |
| `tcp4-multi-sameport13` | ❌ |  |
| `tcp4-multi-sameport14` | ❌ |  |
| `tcp4-uni-basic01` | ❌ |  |
| `tcp4-uni-basic02` | ❌ |  |
| `tcp4-uni-basic03` | ❌ |  |
| `tcp4-uni-basic04` | ❌ |  |
| `tcp4-uni-basic05` | ❌ |  |
| `tcp4-uni-basic06` | ❌ |  |
| `tcp4-uni-basic07` | ❌ |  |
| `tcp4-uni-basic08` | ❌ |  |
| `tcp4-uni-basic09` | ❌ |  |
| `tcp4-uni-basic10` | ❌ |  |
| `tcp4-uni-basic11` | ❌ |  |
| `tcp4-uni-basic12` | ❌ |  |
| `tcp4-uni-basic13` | ❌ |  |
| `tcp4-uni-basic14` | ❌ |  |
| `tcp4-uni-dsackoff01` | ❌ |  |
| `tcp4-uni-dsackoff02` | ❌ |  |
| `tcp4-uni-dsackoff03` | ❌ |  |
| `tcp4-uni-dsackoff04` | ❌ |  |
| `tcp4-uni-dsackoff05` | ❌ |  |
| `tcp4-uni-dsackoff06` | ❌ |  |
| `tcp4-uni-dsackoff07` | ❌ |  |
| `tcp4-uni-dsackoff08` | ❌ |  |
| `tcp4-uni-dsackoff09` | ❌ |  |
| `tcp4-uni-dsackoff10` | ❌ |  |
| `tcp4-uni-dsackoff11` | ❌ |  |
| `tcp4-uni-dsackoff12` | ❌ |  |
| `tcp4-uni-dsackoff13` | ❌ |  |
| `tcp4-uni-dsackoff14` | ❌ |  |
| `tcp4-uni-pktlossdup01` | ❌ |  |
| `tcp4-uni-pktlossdup02` | ❌ |  |
| `tcp4-uni-pktlossdup03` | ❌ |  |
| `tcp4-uni-pktlossdup04` | ❌ |  |
| `tcp4-uni-pktlossdup05` | ❌ |  |
| `tcp4-uni-pktlossdup06` | ❌ |  |
| `tcp4-uni-pktlossdup07` | ❌ |  |
| `tcp4-uni-pktlossdup08` | ❌ |  |
| `tcp4-uni-pktlossdup09` | ❌ |  |
| `tcp4-uni-pktlossdup10` | ❌ |  |
| `tcp4-uni-pktlossdup11` | ❌ |  |
| `tcp4-uni-pktlossdup12` | ❌ |  |
| `tcp4-uni-pktlossdup13` | ❌ |  |
| `tcp4-uni-pktlossdup14` | ❌ |  |
| `tcp4-uni-sackoff01` | ❌ |  |
| `tcp4-uni-sackoff02` | ❌ |  |
| `tcp4-uni-sackoff03` | ❌ |  |
| `tcp4-uni-sackoff04` | ❌ |  |
| `tcp4-uni-sackoff05` | ❌ |  |
| `tcp4-uni-sackoff06` | ❌ |  |
| `tcp4-uni-sackoff07` | ❌ |  |
| `tcp4-uni-sackoff08` | ❌ |  |
| `tcp4-uni-sackoff09` | ❌ |  |
| `tcp4-uni-sackoff10` | ❌ |  |
| `tcp4-uni-sackoff11` | ❌ |  |
| `tcp4-uni-sackoff12` | ❌ |  |
| `tcp4-uni-sackoff13` | ❌ |  |
| `tcp4-uni-sackoff14` | ❌ |  |
| `tcp4-uni-smallsend01` | ❌ |  |
| `tcp4-uni-smallsend02` | ❌ |  |
| `tcp4-uni-smallsend03` | ❌ |  |
| `tcp4-uni-smallsend04` | ❌ |  |
| `tcp4-uni-smallsend05` | ❌ |  |
| `tcp4-uni-smallsend06` | ❌ |  |
| `tcp4-uni-smallsend07` | ❌ |  |
| `tcp4-uni-smallsend08` | ❌ |  |
| `tcp4-uni-smallsend09` | ❌ |  |
| `tcp4-uni-smallsend10` | ❌ |  |
| `tcp4-uni-smallsend11` | ❌ |  |
| `tcp4-uni-smallsend12` | ❌ |  |
| `tcp4-uni-smallsend13` | ❌ |  |
| `tcp4-uni-smallsend14` | ❌ |  |
| `tcp4-uni-tso01` | ❌ |  |
| `tcp4-uni-tso02` | ❌ |  |
| `tcp4-uni-tso03` | ❌ |  |
| `tcp4-uni-tso04` | ❌ |  |
| `tcp4-uni-tso05` | ❌ |  |
| `tcp4-uni-tso06` | ❌ |  |
| `tcp4-uni-tso07` | ❌ |  |
| `tcp4-uni-tso08` | ❌ |  |
| `tcp4-uni-tso09` | ❌ |  |
| `tcp4-uni-tso10` | ❌ |  |
| `tcp4-uni-tso11` | ❌ |  |
| `tcp4-uni-tso12` | ❌ |  |
| `tcp4-uni-tso13` | ❌ |  |
| `tcp4-uni-tso14` | ❌ |  |
| `tcp4-uni-winscale01` | ❌ |  |
| `tcp4-uni-winscale02` | ❌ |  |
| `tcp4-uni-winscale03` | ❌ |  |
| `tcp4-uni-winscale04` | ❌ |  |
| `tcp4-uni-winscale05` | ❌ |  |
| `tcp4-uni-winscale06` | ❌ |  |
| `tcp4-uni-winscale07` | ❌ |  |
| `tcp4-uni-winscale08` | ❌ |  |
| `tcp4-uni-winscale09` | ❌ |  |
| `tcp4-uni-winscale10` | ❌ |  |
| `tcp4-uni-winscale11` | ❌ |  |
| `tcp4-uni-winscale12` | ❌ |  |
| `tcp4-uni-winscale13` | ❌ |  |
| `tcp4-uni-winscale14` | ❌ |  |
| `tcp6-multi-diffip01` | ❌ |  |
| `tcp6-multi-diffip02` | ❌ |  |
| `tcp6-multi-diffip03` | ❌ |  |
| `tcp6-multi-diffip04` | ❌ |  |
| `tcp6-multi-diffip05` | ❌ |  |
| `tcp6-multi-diffip06` | ❌ |  |
| `tcp6-multi-diffip07` | ❌ |  |
| `tcp6-multi-diffip08` | ❌ |  |
| `tcp6-multi-diffip09` | ❌ |  |
| `tcp6-multi-diffip10` | ❌ |  |
| `tcp6-multi-diffip11` | ❌ |  |
| `tcp6-multi-diffip12` | ❌ |  |
| `tcp6-multi-diffip13` | ❌ |  |
| `tcp6-multi-diffip14` | ❌ |  |
| `tcp6-multi-diffnic01` | ❌ |  |
| `tcp6-multi-diffnic02` | ❌ |  |
| `tcp6-multi-diffnic03` | ❌ |  |
| `tcp6-multi-diffnic04` | ❌ |  |
| `tcp6-multi-diffnic05` | ❌ |  |
| `tcp6-multi-diffnic06` | ❌ |  |
| `tcp6-multi-diffnic07` | ❌ |  |
| `tcp6-multi-diffnic08` | ❌ |  |
| `tcp6-multi-diffnic09` | ❌ |  |
| `tcp6-multi-diffnic10` | ❌ |  |
| `tcp6-multi-diffnic11` | ❌ |  |
| `tcp6-multi-diffnic12` | ❌ |  |
| `tcp6-multi-diffnic13` | ❌ |  |
| `tcp6-multi-diffnic14` | ❌ |  |
| `tcp6-multi-diffport01` | ❌ |  |
| `tcp6-multi-diffport02` | ❌ |  |
| `tcp6-multi-diffport03` | ❌ |  |
| `tcp6-multi-diffport04` | ❌ |  |
| `tcp6-multi-diffport05` | ❌ |  |
| `tcp6-multi-diffport06` | ❌ |  |
| `tcp6-multi-diffport07` | ❌ |  |
| `tcp6-multi-diffport08` | ❌ |  |
| `tcp6-multi-diffport09` | ❌ |  |
| `tcp6-multi-diffport10` | ❌ |  |
| `tcp6-multi-diffport11` | ❌ |  |
| `tcp6-multi-diffport12` | ❌ |  |
| `tcp6-multi-diffport13` | ❌ |  |
| `tcp6-multi-diffport14` | ❌ |  |
| `tcp6-multi-sameport01` | ❌ |  |
| `tcp6-multi-sameport02` | ❌ |  |
| `tcp6-multi-sameport03` | ❌ |  |
| `tcp6-multi-sameport04` | ❌ |  |
| `tcp6-multi-sameport05` | ❌ |  |
| `tcp6-multi-sameport06` | ❌ |  |
| `tcp6-multi-sameport07` | ❌ |  |
| `tcp6-multi-sameport08` | ❌ |  |
| `tcp6-multi-sameport09` | ❌ |  |
| `tcp6-multi-sameport10` | ❌ |  |
| `tcp6-multi-sameport11` | ❌ |  |
| `tcp6-multi-sameport12` | ❌ |  |
| `tcp6-multi-sameport13` | ❌ |  |
| `tcp6-multi-sameport14` | ❌ |  |
| `tcp6-uni-basic01` | ❌ |  |
| `tcp6-uni-basic02` | ❌ |  |
| `tcp6-uni-basic03` | ❌ |  |
| `tcp6-uni-basic04` | ❌ |  |
| `tcp6-uni-basic05` | ❌ |  |
| `tcp6-uni-basic06` | ❌ |  |
| `tcp6-uni-basic07` | ❌ |  |
| `tcp6-uni-basic08` | ❌ |  |
| `tcp6-uni-basic09` | ❌ |  |
| `tcp6-uni-basic10` | ❌ |  |
| `tcp6-uni-basic11` | ❌ |  |
| `tcp6-uni-basic12` | ❌ |  |
| `tcp6-uni-basic13` | ❌ |  |
| `tcp6-uni-basic14` | ❌ |  |
| `tcp6-uni-dsackoff01` | ❌ |  |
| `tcp6-uni-dsackoff02` | ❌ |  |
| `tcp6-uni-dsackoff03` | ❌ |  |
| `tcp6-uni-dsackoff04` | ❌ |  |
| `tcp6-uni-dsackoff05` | ❌ |  |
| `tcp6-uni-dsackoff06` | ❌ |  |
| `tcp6-uni-dsackoff07` | ❌ |  |
| `tcp6-uni-dsackoff08` | ❌ |  |
| `tcp6-uni-dsackoff09` | ❌ |  |
| `tcp6-uni-dsackoff10` | ❌ |  |
| `tcp6-uni-dsackoff11` | ❌ |  |
| `tcp6-uni-dsackoff12` | ❌ |  |
| `tcp6-uni-dsackoff13` | ❌ |  |
| `tcp6-uni-dsackoff14` | ❌ |  |
| `tcp6-uni-pktlossdup01` | ❌ |  |
| `tcp6-uni-pktlossdup02` | ❌ |  |
| `tcp6-uni-pktlossdup03` | ❌ |  |
| `tcp6-uni-pktlossdup04` | ❌ |  |
| `tcp6-uni-pktlossdup05` | ❌ |  |
| `tcp6-uni-pktlossdup06` | ❌ |  |
| `tcp6-uni-pktlossdup07` | ❌ |  |
| `tcp6-uni-pktlossdup08` | ❌ |  |
| `tcp6-uni-pktlossdup09` | ❌ |  |
| `tcp6-uni-pktlossdup10` | ❌ |  |
| `tcp6-uni-pktlossdup11` | ❌ |  |
| `tcp6-uni-pktlossdup12` | ❌ |  |
| `tcp6-uni-pktlossdup13` | ❌ |  |
| `tcp6-uni-pktlossdup14` | ❌ |  |
| `tcp6-uni-sackoff01` | ❌ |  |
| `tcp6-uni-sackoff02` | ❌ |  |
| `tcp6-uni-sackoff03` | ❌ |  |
| `tcp6-uni-sackoff04` | ❌ |  |
| `tcp6-uni-sackoff05` | ❌ |  |
| `tcp6-uni-sackoff06` | ❌ |  |
| `tcp6-uni-sackoff07` | ❌ |  |
| `tcp6-uni-sackoff08` | ❌ |  |
| `tcp6-uni-sackoff09` | ❌ |  |
| `tcp6-uni-sackoff10` | ❌ |  |
| `tcp6-uni-sackoff11` | ❌ |  |
| `tcp6-uni-sackoff12` | ❌ |  |
| `tcp6-uni-sackoff13` | ❌ |  |
| `tcp6-uni-sackoff14` | ❌ |  |
| `tcp6-uni-smallsend01` | ❌ |  |
| `tcp6-uni-smallsend02` | ❌ |  |
| `tcp6-uni-smallsend03` | ❌ |  |
| `tcp6-uni-smallsend04` | ❌ |  |
| `tcp6-uni-smallsend05` | ❌ |  |
| `tcp6-uni-smallsend06` | ❌ |  |
| `tcp6-uni-smallsend07` | ❌ |  |
| `tcp6-uni-smallsend08` | ❌ |  |
| `tcp6-uni-smallsend09` | ❌ |  |
| `tcp6-uni-smallsend10` | ❌ |  |
| `tcp6-uni-smallsend11` | ❌ |  |
| `tcp6-uni-smallsend12` | ❌ |  |
| `tcp6-uni-smallsend13` | ❌ |  |
| `tcp6-uni-smallsend14` | ❌ |  |
| `tcp6-uni-tso01` | ❌ |  |
| `tcp6-uni-tso02` | ❌ |  |
| `tcp6-uni-tso03` | ❌ |  |
| `tcp6-uni-tso04` | ❌ |  |
| `tcp6-uni-tso05` | ❌ |  |
| `tcp6-uni-tso06` | ❌ |  |
| `tcp6-uni-tso07` | ❌ |  |
| `tcp6-uni-tso08` | ❌ |  |
| `tcp6-uni-tso09` | ❌ |  |
| `tcp6-uni-tso10` | ❌ |  |
| `tcp6-uni-tso11` | ❌ |  |
| `tcp6-uni-tso12` | ❌ |  |
| `tcp6-uni-tso13` | ❌ |  |
| `tcp6-uni-tso14` | ❌ |  |
| `tcp6-uni-winscale01` | ❌ |  |
| `tcp6-uni-winscale02` | ❌ |  |
| `tcp6-uni-winscale03` | ❌ |  |
| `tcp6-uni-winscale04` | ❌ |  |
| `tcp6-uni-winscale05` | ❌ |  |
| `tcp6-uni-winscale06` | ❌ |  |
| `tcp6-uni-winscale07` | ❌ |  |
| `tcp6-uni-winscale08` | ❌ |  |
| `tcp6-uni-winscale09` | ❌ |  |
| `tcp6-uni-winscale10` | ❌ |  |
| `tcp6-uni-winscale11` | ❌ |  |
| `tcp6-uni-winscale12` | ❌ |  |
| `tcp6-uni-winscale13` | ❌ |  |
| `tcp6-uni-winscale14` | ❌ |  |
| `tcp_cc_lib.sh` | ❌ |  |
| `tcp_fastopen_run.sh` | ❌ |  |
| `tcp_ipsec.sh` | ❌ |  |
| `tcp_ipsec_vti.sh` | ❌ |  |
| `tcpdump01.sh` | ❌ |  |
| `test.sh` | ❌ |  |
| `test_1_to_1_accept_close` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_addrs` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_connect` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_connectx` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_events` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_nonblock` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_recvfrom` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_recvmsg` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_rtoinfo` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_send` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_sendmsg` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_sendto` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_shutdown` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_socket_bind_listen` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_sockopt` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_1_to_1_threads` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_assoc_abort` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_assoc_shutdown` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_autoclose` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_basic` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_basic_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_connect` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_connectx` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_controllers.sh` | ❌ |  |
| `test_fragments` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_fragments_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_getname` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_getname_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_inaddr_any` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_inaddr_any_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_peeloff` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_peeloff_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_recvmsg` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_robind.sh` | ❌ |  |
| `test_sctp_sendrecvmsg` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_sctp_sendrecvmsg_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_sockopt` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_sockopt_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_tcp_style` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_tcp_style_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_timetolive` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `test_timetolive_v6` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `testsf_c` | ❌ |  |
| `testsf_c6` | ❌ |  |
| `testsf_s` | ❌ |  |
| `testsf_s6` | ❌ |  |
| `time-schedule` | ❌ |  |
| `timed_forkbomb` | ❌ |  |
| `tpci` | ❌ |  |
| `tpm_changeauth_tests.sh` | ❌ |  |
| `tpm_changeauth_tests_exp01.sh` | ❌ |  |
| `tpm_changeauth_tests_exp02.sh` | ❌ |  |
| `tpm_changeauth_tests_exp03.sh` | ❌ |  |
| `tpm_clear_tests.sh` | ❌ |  |
| `tpm_clear_tests_exp01.sh` | ❌ |  |
| `tpm_getpubek_tests.sh` | ❌ |  |
| `tpm_getpubek_tests_exp01.sh` | ❌ |  |
| `tpm_restrictpubek_tests.sh` | ❌ |  |
| `tpm_restrictpubek_tests_exp01.sh` | ❌ |  |
| `tpm_restrictpubek_tests_exp02.sh` | ❌ |  |
| `tpm_restrictpubek_tests_exp03.sh` | ❌ |  |
| `tpm_selftest_tests.sh` | ❌ |  |
| `tpm_takeownership_tests.sh` | ❌ |  |
| `tpm_takeownership_tests_exp01.sh` | ❌ |  |
| `tpm_version_tests.sh` | ❌ |  |
| `tpmtoken_import_tests.sh` | ❌ |  |
| `tpmtoken_import_tests_exp01.sh` | ❌ |  |
| `tpmtoken_import_tests_exp02.sh` | ❌ |  |
| `tpmtoken_import_tests_exp03.sh` | ❌ |  |
| `tpmtoken_import_tests_exp04.sh` | ❌ |  |
| `tpmtoken_import_tests_exp05.sh` | ❌ |  |
| `tpmtoken_import_tests_exp06.sh` | ❌ |  |
| `tpmtoken_import_tests_exp07.sh` | ❌ |  |
| `tpmtoken_import_tests_exp08.sh` | ❌ |  |
| `tpmtoken_init_tests.sh` | ❌ |  |
| `tpmtoken_init_tests_exp00.sh` | ❌ |  |
| `tpmtoken_init_tests_exp01.sh` | ❌ |  |
| `tpmtoken_init_tests_exp02.sh` | ❌ |  |
| `tpmtoken_init_tests_exp03.sh` | ❌ |  |
| `tpmtoken_objects_tests.sh` | ❌ |  |
| `tpmtoken_objects_tests_exp01.sh` | ❌ |  |
| `tpmtoken_protect_tests.sh` | ❌ |  |
| `tpmtoken_protect_tests_exp01.sh` | ❌ |  |
| `tpmtoken_protect_tests_exp02.sh` | ❌ |  |
| `tpmtoken_setpasswd_tests.sh` | ❌ |  |
| `tpmtoken_setpasswd_tests_exp01.sh` | ❌ |  |
| `tpmtoken_setpasswd_tests_exp02.sh` | ❌ |  |
| `tpmtoken_setpasswd_tests_exp03.sh` | ❌ |  |
| `tpmtoken_setpasswd_tests_exp04.sh` | ❌ |  |
| `trace_sched` | ❌ |  |
| `truncate03` | ❌ |  |
| `truncate03_64` | ❌ |  |
| `tst_ansi_color.sh` | ❌ |  |
| `tst_brk` | ❌ |  |
| `tst_brkm` | ❌ |  |
| `tst_cgctl` | ❌ |  |
| `tst_check_drivers` | ❌ |  |
| `tst_check_kconfigs` | ❌ |  |
| `tst_checkpoint` | ❌ |  |
| `tst_device` | ❌ |  |
| `tst_exit` | ❌ |  |
| `tst_fs_has_free` | ❌ |  |
| `tst_fsfreeze` | ❌ |  |
| `tst_get_free_pids` | ❌ |  |
| `tst_get_median` | ❌ |  |
| `tst_get_unused_port` | ❌ |  |
| `tst_getconf` | ❌ |  |
| `tst_hexdump` | ❌ |  |
| `tst_kvcmp` | ❌ |  |
| `tst_lockdown_enabled` | ❌ |  |
| `tst_ncpus` | ❌ |  |
| `tst_ncpus_conf` | ❌ |  |
| `tst_ncpus_max` | ❌ |  |
| `tst_net.sh` | ❌ |  |
| `tst_net_iface_prefix` | ❌ |  |
| `tst_net_ip_prefix` | ❌ |  |
| `tst_net_stress.sh` | ❌ |  |
| `tst_net_vars` | ❌ |  |
| `tst_ns_create` | ❌ |  |
| `tst_ns_exec` | ❌ |  |
| `tst_ns_ifmove` | ❌ |  |
| `tst_random` | ❌ |  |
| `tst_res` | ❌ |  |
| `tst_resm` | ❌ |  |
| `tst_rod` | ❌ |  |
| `tst_secureboot_enabled` | ❌ |  |
| `tst_security.sh` | ❌ |  |
| `tst_sleep` | ❌ |  |
| `tst_supported_fs` | ❌ |  |
| `tst_test.sh` | ❌ |  |
| `tst_timeout_kill` | ❌ |  |
| `uaccess` | ❌ |  |
| `udp4-multi-diffip01` | ❌ |  |
| `udp4-multi-diffip02` | ❌ |  |
| `udp4-multi-diffip03` | ❌ |  |
| `udp4-multi-diffip04` | ❌ |  |
| `udp4-multi-diffip05` | ❌ |  |
| `udp4-multi-diffip06` | ❌ |  |
| `udp4-multi-diffip07` | ❌ |  |
| `udp4-multi-diffnic01` | ❌ |  |
| `udp4-multi-diffnic02` | ❌ |  |
| `udp4-multi-diffnic03` | ❌ |  |
| `udp4-multi-diffnic04` | ❌ |  |
| `udp4-multi-diffnic05` | ❌ |  |
| `udp4-multi-diffnic06` | ❌ |  |
| `udp4-multi-diffnic07` | ❌ |  |
| `udp4-multi-diffport01` | ❌ |  |
| `udp4-multi-diffport02` | ❌ |  |
| `udp4-multi-diffport03` | ❌ |  |
| `udp4-multi-diffport04` | ❌ |  |
| `udp4-multi-diffport05` | ❌ |  |
| `udp4-multi-diffport06` | ❌ |  |
| `udp4-multi-diffport07` | ❌ |  |
| `udp4-uni-basic01` | ❌ |  |
| `udp4-uni-basic02` | ❌ |  |
| `udp4-uni-basic03` | ❌ |  |
| `udp4-uni-basic04` | ❌ |  |
| `udp4-uni-basic05` | ❌ |  |
| `udp4-uni-basic06` | ❌ |  |
| `udp4-uni-basic07` | ❌ |  |
| `udp6-multi-diffip01` | ❌ |  |
| `udp6-multi-diffip02` | ❌ |  |
| `udp6-multi-diffip03` | ❌ |  |
| `udp6-multi-diffip04` | ❌ |  |
| `udp6-multi-diffip05` | ❌ |  |
| `udp6-multi-diffip06` | ❌ |  |
| `udp6-multi-diffip07` | ❌ |  |
| `udp6-multi-diffnic01` | ❌ |  |
| `udp6-multi-diffnic02` | ❌ |  |
| `udp6-multi-diffnic03` | ❌ |  |
| `udp6-multi-diffnic04` | ❌ |  |
| `udp6-multi-diffnic05` | ❌ |  |
| `udp6-multi-diffnic06` | ❌ |  |
| `udp6-multi-diffnic07` | ❌ |  |
| `udp6-multi-diffport01` | ❌ |  |
| `udp6-multi-diffport02` | ❌ |  |
| `udp6-multi-diffport03` | ❌ |  |
| `udp6-multi-diffport04` | ❌ |  |
| `udp6-multi-diffport05` | ❌ |  |
| `udp6-multi-diffport06` | ❌ |  |
| `udp6-multi-diffport07` | ❌ |  |
| `udp6-uni-basic01` | ❌ |  |
| `udp6-uni-basic02` | ❌ |  |
| `udp6-uni-basic03` | ❌ |  |
| `udp6-uni-basic04` | ❌ |  |
| `udp6-uni-basic05` | ❌ |  |
| `udp6-uni-basic06` | ❌ |  |
| `udp6-uni-basic07` | ❌ |  |
| `udp_ipsec.sh` | ❌ |  |
| `udp_ipsec_vti.sh` | ❌ |  |
| `uevent01` | ❌ |  |
| `uevent03` | ❌ |  |
| `ulimit01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `umount01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `umount02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `umount03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `umount2_01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `umount2_02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `unlink09` | ❌ | 沙箱测不准:需 loop 块设备 |
| `userfaultfd01` | ❌ |  |
| `userns06_capcheck` | ❌ |  |
| `userns07` | ❌ | 沙箱测不准:需写全局 sysctl |
| `userns08` | ❌ | 沙箱测不准:需写全局 sysctl |
| `utime01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `utime02` | ❌ | 沙箱测不准:需 loop 块设备 |
| `utime03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `utime04` | ❌ | 沙箱测不准:需 loop 块设备 |
| `utime05` | ❌ | 沙箱测不准:需 loop 块设备 |
| `utime06` | ❌ |  |
| `utimensat01` | ❌ | 沙箱测不准:需 loop 块设备 |
| `utimes01` | ❌ |  |
| `utsname02` | ❌ |  |
| `utsname04` | ❌ |  |
| `verify_caps_exec` | ❌ |  |
| `vfork` | ❌ |  |
| `vfork01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `vfork02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `vfork_freeze.sh` | ❌ |  |
| `vhangup01` | ❌ |  |
| `vhangup02` | ❌ |  |
| `virt_lib.sh` | ❌ |  |
| `vlan03.sh` | ❌ |  |
| `vma01` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `vma02` | ❌ |  |
| `vma03` | ❌ |  |
| `vma04` | ❌ |  |
| `vma05_vdso` | ❌ |  |
| `vxlan03.sh` | ❌ |  |
| `vxlan04.sh` | ❌ |  |
| `wireguard01.sh` | ❌ |  |
| `wireguard02.sh` | ❌ |  |
| `wireguard_lib.sh` | ❌ |  |
| `write_freezing.sh` | ❌ |  |
| `writetest` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `writev02` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `writev03` | ❌ | 沙箱测不准:需 loop 块设备 |
| `writev05` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `writev06` | ❌ | legacy 框架(有 TPASS 行但官方不认) |
| `zram01.sh` | ❌ |  |
| `zram02.sh` | ❌ |  |
| `zram03` | ❌ |  |
| `zram_lib.sh` | ❌ |  |

## 收尾

- 镜像里有但宿主无法构建: `prctl04`（1 个，未计入）。
- **算分总计：musl 口径 8594 分 / glibc 口径 8574 分（1028 个文件）。**
- 原始数据: `target/ltp-full-sweep/`(results/*.log, judged-full.json)。
