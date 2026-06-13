# 非白名单 LTP 扫描汇总（2026-06-11）

## 方法
- 全量官方镜像 2821 文件 − 当前白名单 595 = **非白名单 2227** 个，逐个无参执行。
- 5 个一组，每组外层 60s 墙钟超时杀 hang；**不带 `-I`（单次执行＝官方计分口径）**；passed = 该用例 `Summary: passed N`（judge 取分口径，>0 即可加入白名单候选）。
- 三遍扫描：主扫全量 → 复扫 notrun(hang 连带) → 第三遍 still-notrun(剔 hang/cgroup)。
- **cgroup 类全部排除**（cgroup/memcg/cpuset/cpuctl/cfs_bandwidth 共 59 个，内核未实现，跑必 hang）。
- 原始逐组记录见 `nonwhitelist-scan-{rv,la}.md` / `nonwhitelist-rescan{,2}-{rv,la}.md`。

## 结果总览
| 架构 | passed>0 候选 | 分数合计 |
|---|---|---|
| **RV** | **92** | **528** |
| **LA** | **50** | **161** |

- 两架构都过：48；仅 RV 过：44（基本是 `fs_bind*.sh` 挂载绑定测试）；仅 LA 过：2。
- **RV/LA 白名单可不同**：fs_bind*.sh 只加 RV；网络 syscall 两边都加。
- ⚠️ caveat：passed 是内核自报 Summary.passed；加入 submit 白名单后需用官方 judge 实跑确认（条件与隔离单跑可能略有差）。
- 三遍后仍 notrun（hang 邻接，未测到，可后续单独复扫）：45 个。

## RV 候选（92 个，528 分，按分降序）
| case | passed |
|---|---|
| bind04 | 16 |
| bind05 | 14 |
| fs_bind_rbind27.sh | 14 |
| fs_bind05.sh | 13 |
| fs_bind07.sh | 13 |
| fs_bind_rbind11.sh | 13 |
| fs_bind_rbind15.sh | 11 |
| fs_bind_rbind22.sh | 11 |
| fs_bind_rbind25.sh | 11 |
| fs_bind_rbind28.sh | 11 |
| fs_bind_rbind37.sh | 11 |
| fs_bind06.sh | 10 |
| fs_bind08.sh | 10 |
| fs_bind_rbind23.sh | 10 |
| fs_bind_rbind24.sh | 10 |
| fs_bind_rbind33.sh | 10 |
| recvmsg01 | 10 |
| socketpair01 | 10 |
| fs_bind19.sh | 9 |
| fs_bind_move09.sh | 9 |
| fs_bind_rbind10.sh | 9 |
| fs_bind_rbind13.sh | 9 |
| fs_bind_rbind19.sh | 9 |
| fs_bind_rbind29.sh | 9 |
| fs_bind_rbind31.sh | 9 |
| fs_bind_rbind38.sh | 9 |
| getsockopt01 | 9 |
| accept4_01 | 8 |
| fs_bind04.sh | 8 |
| fs_bind_move01.sh | 8 |
| fs_bind_move02.sh | 8 |
| fs_bind_move03.sh | 8 |
| fs_bind_move11.sh | 8 |
| fs_bind_rbind21.sh | 8 |
| bind01 | 7 |
| fcntl36 | 7 |
| fcntl36_64 | 7 |
| fs_bind17.sh | 7 |
| fs_bind18.sh | 7 |
| fs_bind_move12.sh | 7 |
| fs_bind_rbind14.sh | 7 |
| fs_bind_rbind16.sh | 7 |
| fs_bind_rbind17.sh | 7 |
| fs_bind_rbind18.sh | 7 |
| fs_bind_rbind20.sh | 7 |
| fs_bind_rbind30.sh | 7 |
| fs_bind_rbind32.sh | 7 |
| getpeername01 | 7 |
| fs_bind_regression.sh | 6 |
| fs_bind_rbind39.sh | 5 |
| in6_01 | 5 |
| fs_bind_cloneNS03.sh | 4 |
| mkdir_tests.sh | 4 |
| send02 | 4 |
| sendmmsg01 | 4 |
| sendmmsg02 | 4 |
| socket02 | 4 |
| socketpair02 | 4 |
| bind03 | 3 |
| semtest_2ns | 2 |
| setgroups03 | 2 |
| setsockopt02 | 2 |
| utsname02 | 2 |
| utsname04 | 2 |
| accept02 | 1 |
| bind02 | 1 |
| connect02 | 1 |
| cve-2017-17052 | 1 |
| fork_procs | 1 |
| fsx-linux | 1 |
| futex_wait03 | 1 |
| generate_lvm_runfile.sh | 1 |
| getsockopt02 | 1 |
| mesgq_nstest | 1 |
| mmapstress01 | 1 |
| mmapstress04 | 1 |
| mq_notify03 | 1 |
| mqns_01 | 1 |
| mqns_02 | 1 |
| recvmmsg01 | 1 |
| recvmsg02 | 1 |
| recvmsg03 | 1 |
| sem_comm | 1 |
| sem_nstest | 1 |
| setsockopt04 | 1 |
| setsockopt10 | 1 |
| shm_comm | 1 |
| shmem_2nstest | 1 |
| shmnstest | 1 |
| tgkill01 | 1 |
| thp01 | 1 |
| utsname01 | 1 |

## LA 候选（50 个，161 分，按分降序）
| case | passed |
|---|---|
| bind04 | 16 |
| bind05 | 14 |
| recvmsg01 | 10 |
| socketpair01 | 10 |
| getsockopt01 | 9 |
| accept4_01 | 8 |
| bind01 | 7 |
| fcntl36 | 7 |
| fcntl36_64 | 7 |
| getpeername01 | 7 |
| in6_01 | 5 |
| send02 | 4 |
| sendmmsg01 | 4 |
| sendmmsg02 | 4 |
| socket02 | 4 |
| socketpair02 | 4 |
| bind03 | 3 |
| semtest_2ns | 2 |
| setgroups03 | 2 |
| setsockopt02 | 2 |
| utsname02 | 2 |
| utsname04 | 2 |
| accept02 | 1 |
| bind02 | 1 |
| connect02 | 1 |
| cve-2017-17052 | 1 |
| fcntl34 | 1 |
| fcntl34_64 | 1 |
| fork_procs | 1 |
| fsx-linux | 1 |
| futex_wait03 | 1 |
| getsockopt02 | 1 |
| mesgq_nstest | 1 |
| mmapstress01 | 1 |
| mmapstress04 | 1 |
| mqns_01 | 1 |
| mqns_02 | 1 |
| recvmmsg01 | 1 |
| recvmsg02 | 1 |
| recvmsg03 | 1 |
| sem_comm | 1 |
| sem_nstest | 1 |
| setsockopt04 | 1 |
| setsockopt10 | 1 |
| shm_comm | 1 |
| shmem_2nstest | 1 |
| shmnstest | 1 |
| tgkill01 | 1 |
| thp01 | 1 |
| utsname01 | 1 |

## 三遍后仍 notrun（45 个，非 cgroup）
clock_gettime01, clock_gettime04, dirtyc0w_shmem, fork14, fs_bind10.sh, fs_bind11.sh, fs_bind15.sh, fs_bind16.sh, fs_bind23.sh, fs_bind24.sh, fs_bind_cloneNS01.sh, fs_bind_cloneNS05.sh, fs_bind_cloneNS07.sh, fs_bind_move06.sh, fs_bind_move07.sh, fs_bind_move16.sh, fs_bind_move18.sh, fs_bind_move19.sh, fs_bind_move22.sh, fs_bind_rbind01.sh, fs_bind_rbind04.sh, fs_bind_rbind06.sh, fs_bind_rbind07-2.sh, fs_bind_rbind08.sh, futex_cmp_requeue01, getrusage03, getrusage04, kcmp03, kill10, kill11, msgrcv05, msgrcv06, msgsnd05, msgsnd06, ping01.sh, rename14, shmctl01, sigtimedwait01, sigwaitinfo01, tst_kvcmp, wait401, waitid07, waitid08, waitpid07, waitpid11

---

## 候选完整测试验证（2026-06-11，musl+glibc 连续跑）

把候选**连续整批跑**（贴近官方提交一次性顺序跑）后发现：**隔离扫描的分数被 .sh 类严重高估**。

| | musl | glibc | 用例 | 状态 |
|---|---|---|---|---|
| **RV 干净集（非 .sh）** | **159** | **167** | 48–49 网络/syscall | 49/49 与 48/48 干净 exit:0 |
| **LA** | **159** | **168** | 50 网络/syscall | 50/50 干净（拆 2 段避 ~512B cmdline 限制） |

### 关键发现（连续跑才暴露）
- **`.sh` 类不可加**：`fs_bind*.sh`(41,363分) umount 互相干扰崩；`mkdir_tests.sh` 死循环读 /proc；`generate_lvm_runfile.sh` 探测 fs 挂死。隔离分组（且每组恢复镜像）才拿得到分，**连续跑会卡死整轮、拖垮提交**。RV 扫描的 528 分里 ~368 是这些虚分。
- **`mq_notify03`**：glibc 下 `mq_notify` EINVAL 后挂死，已排除。
- **LA `-append` cmdline ~512B 上限**（RV 是 16K），LA 候选要拆段跑。

### 现实可加（按架构，白名单可不同）
- **RV**：48 个非 .sh（网络/syscall）→ +159 musl / +167 glibc
- **LA**：50 个（网络/syscall）→ +159 musl / +168 glibc
- 输出原文：`target/oscomp/debug_rv.txt`、`target/oscomp/debug_la.txt`

### 后续机会
- fs_bind*.sh 的 ~363 RV 分需要先修内核 umount（EINVAL）/挂载隔离，修好后可解锁。
