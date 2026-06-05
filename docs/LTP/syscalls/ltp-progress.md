# LTP P0 Progress

记录当前 `ltp-musl` p0 用例的已知通过情况，避免反复跑已经稳定通过的 case。

当前基准来自 2026-05-23 的 p0 跑分：`550/604`。个别 case 后续用单测确认过修复，表中记录“当前已知最好且相对稳定”的结果。网络/socket 相关暂缓，单独记录在 `ltp-network-deferred.md`。

2026-05-23 重新跑未全过项：`78/122`。按下方表格逐项相加，当前记录总分为 `550/621`。注意这个总分混合了 full p0 和 focused 单测分母，适合做进度记录，不等同于一次完整 p0 跑分。剩余错误多数属于 time namespace、AIO、hugetlbfs、短超时精度或信号/超时交互，短期不适合作为主线优先项。

2026-05-25 合并 main 后重跑完整 p0：当前跑到 `setitimer02`，judge 结果为 `532/593`。`eventfd03` 的 `OpenFile::rnode()` panic 已修复并通过；`futex_wake03` 也已通过，原因是 futex wait 的 per-waiter 可观测状态恢复后，父进程可以稳定看到子进程 sleep。后续单测确认 `timerfd01` 的 timerfd-backed `poll()` / `fcntl(F_SETFL, O_NONBLOCK)` panic 已修复，当前剩余为 timerfd tick/readiness 精度问题。

后续优先只跑未全过或暂缓项；完整 p0 命令是 `make oscomp-local-rv64-ltp-batch LTP_BATCH=p0`。

| Area | Case | Status | Score | Note |
| --- | --- | --- | --- | --- |
| alarm | alarm02 | PASS | 6/6 |  |
| alarm | alarm03 | PASS | 2/2 |  |
| alarm | alarm05 | PASS | 3/3 |  |
| alarm | alarm06 | PASS | 2/2 |  |
| alarm | alarm07 | PASS | 2/2 |  |
| clock | clock_nanosleep01 | PARTIAL | 11/14 | `BAD_TS_ADDR_REM` 返回 0，期望 `-1/EFAULT`；bad remaining-timespec 指针未检查 |
| clock | clock_nanosleep02 | PASS | 7/7 | `systemd-detect-virt` 缺失仅打印，不影响通过 |
| clock | clock_nanosleep03 | DEFER | 0/2 | `unshare(128)` 返回 `EINVAL`，time namespace 未实现 |
| clock | clock_nanosleep04 | PASS | 4/4 |  |
| epoll | epoll_ctl01 | PASS | 3/3 |  |
| epoll | epoll_ctl02 | PASS | 9/9 |  |
| epoll | epoll_ctl03 | PASS | 256/256 |  |
| epoll | epoll_ctl04 | PASS | 1/1 |  |
| epoll | epoll_ctl05 | PASS | 1/1 |  |
| epoll | epoll_wait01 | PASS | 3/3 |  |
| epoll | epoll_wait02 | PASS | 7/7 |  |
| epoll | epoll_wait03 | PASS | 5/5 |  |
| epoll | epoll_wait04 | FAIL | 0/1 | timeout=0 仍等待约 2.5ms，短超时 fast path/调度开销问题 |
| epoll | epoll_wait06 | PASS | 9/9 |  |
| epoll | epoll_wait07 | PASS | 5/5 |  |
| eventfd | eventfd01 | PASS | 4/4 | 单测确认 |
| eventfd | eventfd02 | PASS | 5/5 | 单测确认 |
| eventfd | eventfd03 | PASS | 3/3 | 2026-05-25 p0/单测确认；原因是 `pselect` 对 eventfd 误走 VFS `rnode()`，已改为 eventfd readiness 分支 |
| eventfd | eventfd04 | PASS | 3/3 | 单测确认 |
| eventfd | eventfd05 | PASS | 2/2 | 单测确认 |
| eventfd | eventfd06 | DEFER | 0/1 | `libaio is not available`，AIO 暂缓 |
| eventfd | eventfd2_01 | PASS | 2/2 |  |
| eventfd | eventfd2_02 | PASS | 2/2 |  |
| eventfd | eventfd2_03 | PASS | 2/2 |  |
| futex | futex_wait01 | PASS | 4/4 |  |
| futex | futex_wait02 | PASS | 1/1 |  |
| futex | futex_wait03 | PASS | 1/1 |  |
| futex | futex_wait04 | PASS | 1/1 |  |
| futex | futex_wait05 | PASS | 7/7 | 2026-05-25 p0 确认 |
| futex | futex_wake01 | PASS | 6/6 |  |
| futex | futex_wake02 | BROKEN | 0/1 | `Failed to open /proc/<pid>/task/<tid>/stat: ENOENT`；线程 task stat 生命周期/可见性问题 |
| futex | futex_wake03 | PASS | 11/11 | 2026-05-25 p0 确认；恢复 futex per-waiter 可观测状态后，父进程能看到子进程进入 sleep |
| futex | futex_wake04 | DEFER | 0/1 | `hugetlbfs is not supported`，hugepage 暂缓 |
| getitimer | getitimer01 | PASS | 30/30 |  |
| getitimer | getitimer02 | PASS | 3/3 |  |
| nanosleep | nanosleep01 | PASS | 7/7 |  |
| nanosleep | nanosleep02 | PASS | 2/2 | 单测确认 |
| nanosleep | nanosleep04 | PASS | 3/3 |  |
| poll | poll01 | PASS | 2/2 |  |
| poll | poll02 | PASS | 7/7 |  |
| ppoll | ppoll01 | PARTIAL | 18/20 | TIMEOUT case 被 EINTR 打断，信号/超时交互 |
| pselect | pselect01 | PARTIAL | 1/7 | 2026-05-25 p0 结果退化；需要查本轮信号 mask/timeout 交互 |
| pselect | pselect01_64 | PARTIAL | 1/7 | 2026-05-25 p0 结果退化；需要查本轮信号 mask/timeout 交互 |
| pselect | pselect02 | PASS | 3/3 |  |
| pselect | pselect02_64 | PASS | 3/3 |  |
| pselect | pselect03 | PASS | 1/1 |  |
| pselect | pselect03_64 | PASS | 1/1 |  |
| select | select01 | PARTIAL | 6/13 | 2026-05-25 p0 结果 |
| select | select02 | PARTIAL | 13/17 | 2026-05-25 p0 结果 |
| select | select03 | PARTIAL | 16/40 | focused 仍为 16/40，主要是 unsupported variants/TCONF 分母 |
| select | select04 | PARTIAL | 4/7 | focused 仍为 4/7，主要是 unsupported variants/TCONF 分母 |
| setitimer | setitimer01 | PASS | 18/18 |  |
| setitimer | setitimer02 | PASS | 3/3 |  |
| timerfd | timerfd01 | PARTIAL | 3/12 | 2026-05-25 单测确认 panic 已修复；剩余 `no ticks happened` / tick count 偏差，需继续查 timerfd poll deadline 与 expiration 计数 |
| timerfd | timerfd02 | PASS | 6/6 | 2026-05-25 单测确认 |
| timerfd | timerfd04 | FAIL | 0/1 | 2026-05-25 单测确认；`unshare(128)` 返回 `EINVAL`，time namespace 未实现 |
| timerfd | timerfd_create01 | PASS | 2/2 | 2026-05-25 单测确认 |
| timerfd | timerfd_gettime01 | PASS | 3/3 | 2026-05-25 单测确认 |
| timerfd | timerfd_settime01 | PASS | 4/4 | 2026-05-25 单测确认 |
