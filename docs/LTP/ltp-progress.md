# LTP P0 Progress

记录当前 `ltp-musl` p0 用例的已知通过情况，避免反复跑已经稳定通过的 case。

当前基准来自 2026-05-23 的 p0 跑分：`550/604`。个别 case 后续用单测确认过修复，表中记录“当前已知最好且相对稳定”的结果。网络/socket 相关暂缓，单独记录在 `ltp-network-deferred.md`。

2026-05-23 重新跑未全过项：`78/122`。按下方表格逐项相加，当前记录总分为 `594/636`。注意这个总分混合了 full p0 和 focused 单测分母，适合做进度记录，不等同于一次完整 p0 跑分。剩余错误多数属于 time namespace、AIO、hugetlbfs、短超时精度或信号/超时交互，短期不适合作为主线优先项。

后续优先只跑未全过或暂缓项；完整 p0 命令是 `make oscomp-local-rv64-ltp-batch LTP_BATCH=p0`。

| Area | Case | Status | Score | Note |
| --- | --- | --- | --- | --- |
| alarm | alarm02 | PASS | 6/6 |  |
| alarm | alarm03 | PASS | 2/2 |  |
| alarm | alarm05 | PASS | 3/3 |  |
| alarm | alarm06 | PASS | 2/2 |  |
| alarm | alarm07 | PASS | 2/2 |  |
| clock | clock_nanosleep01 | PARTIAL | 12/14 | focused 仍为 12/14，剩余低优先级 |
| clock | clock_nanosleep02 | PASS | 7/7 |  |
| clock | clock_nanosleep03 | FAIL | 0/1 | `unshare(CLONE_NEWTIME)` ENOSYS，time namespace |
| clock | clock_nanosleep04 | PASS | 4/4 |  |
| epoll | epoll_ctl01 | PASS | 3/3 |  |
| epoll | epoll_ctl02 | PASS | 9/9 |  |
| epoll | epoll_ctl03 | PASS | 256/256 |  |
| epoll | epoll_ctl04 | PASS | 1/1 |  |
| epoll | epoll_ctl05 | PASS | 1/1 |  |
| epoll | epoll_wait01 | PASS | 3/3 |  |
| epoll | epoll_wait02 | PASS | 7/7 |  |
| epoll | epoll_wait03 | PASS | 5/5 |  |
| epoll | epoll_wait04 | FAIL | 0/1 | timeout=0 测到约 3.4ms，短超时精度/调度开销 |
| epoll | epoll_wait06 | PASS | 9/9 |  |
| epoll | epoll_wait07 | PASS | 5/5 |  |
| eventfd | eventfd01 | PASS | 4/4 | 单测确认 |
| eventfd | eventfd02 | PASS | 5/5 | 单测确认 |
| eventfd | eventfd03 | PASS | 3/3 | 单测确认 |
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
| futex | futex_wait05 | PASS | 7/7 |  |
| futex | futex_wake01 | PASS | 6/6 |  |
| futex | futex_wake02 | PASS | 11/11 |  |
| futex | futex_wake03 | PASS | 11/11 |  |
| futex | futex_wake04 | DEFER | 0/1 | `hugetlbfs is not supported`，hugepage 暂缓 |
| getitimer | getitimer01 | PASS | 30/30 |  |
| getitimer | getitimer02 | PASS | 3/3 |  |
| nanosleep | nanosleep01 | PASS | 7/7 |  |
| nanosleep | nanosleep02 | PASS | 2/2 | 单测确认 |
| nanosleep | nanosleep04 | PASS | 3/3 |  |
| poll | poll01 | PASS | 2/2 |  |
| poll | poll02 | PASS | 7/7 |  |
| ppoll | ppoll01 | PARTIAL | 18/20 | TIMEOUT case 被 EINTR 打断，信号/超时交互 |
| pselect | pselect01 | PASS | 7/7 |  |
| pselect | pselect01_64 | PASS | 7/7 |  |
| pselect | pselect02 | PASS | 3/3 |  |
| pselect | pselect02_64 | PASS | 3/3 |  |
| pselect | pselect03 | PASS | 1/1 |  |
| pselect | pselect03_64 | PASS | 1/1 |  |
| select | select01 | PARTIAL | 16/19 | focused 结果；旧 full p0 为 0/2 |
| select | select02 | PARTIAL | 14/17 | full p0 14/17；focused 本轮 12/17，短采样会波动 |
| select | select03 | PARTIAL | 16/40 | focused 仍为 16/40，主要是 unsupported variants/TCONF 分母 |
| select | select04 | PARTIAL | 4/7 | focused 仍为 4/7，主要是 unsupported variants/TCONF 分母 |
| setitimer | setitimer01 | PASS | 18/18 |  |
| setitimer | setitimer02 | PASS | 3/3 |  |
| timerfd | timerfd01 | PASS | 12/12 | 单测确认 |
| timerfd | timerfd02 | PASS | 6/6 |  |
| timerfd | timerfd04 | FAIL | 0/1 | `unshare(CLONE_NEWTIME)` ENOSYS，time namespace |
| timerfd | timerfd_create01 | PASS | 2/2 |  |
| timerfd | timerfd_gettime01 | PASS | 3/3 |  |
| timerfd | timerfd_settime01 | PASS | 4/4 |  |
