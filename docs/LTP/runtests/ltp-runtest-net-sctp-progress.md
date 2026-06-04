# LTP `net.sctp` 测试记录表

Date: 2026-06-04

`net.sctp`(41 个条目)的逐测试进度跟踪。实现计划与协议讲解见本地稿
`msp/sctp-implementation-plan-2026-06-04-zh.md`(不提交)。

计分口径同 `ltp-runtest-network-progress.md`:`TPASS` 计 case;`TCONF` 是跳过
(不入分母);本表"case 数"是源码里静态 `tst_resm(TPASS,…)` 计数(运行时含循环会更多)。

## 当前状态

**阶段 0 已应用(开门 + STREAM/SEQPACKET socket 映射)。**

- 门已开:`modules.builtin`/`modules.dep` 加了 `kernel/net/sctp/sctp.ko`
  (`tst_check_driver("sctp")` 通过);`socket(AF_INET/INET6, STREAM|SEQPACKET,
  IPPROTO_SCTP)` → `SocketKind::Sctp`。
- **`test_1_to_1_accept_close` 已通过**(1-to-1 socket/bind/listen/connect/accept/
  close 全程工作,10 case)——证明现有 loopback-TCP 脚手架撑得住 1-to-1 生命周期。
- 阶段 0 重跑(340s 内跑了 8/41)的真实 blocker:
  - **SCTP sockopt 返回 ENOPROTOOPT**(SCTP_EVENTS / SCTP_INITMSG / …)——头号阻塞。
  - `sctp_getladdrs/getpaddrs` 报错(多宿主地址 API)。
  - `connect`/`connectx` 边界 errno 不对(invalid family/length)。
  - 非阻塞 connect 给 EAGAIN(期望 0/EINPROGRESS)。
- ⚠️ 注意:开门后,未实现的部分从 TCONF 变成 FAIL,且个别测试在某操作上阻塞到
  30s 内部超时——这是 SCTP 建设期的中间状态(随阶段 1/2/3 翻成 TPASS)。
- witness:`target/oscomp/ltp-net-sctp-probe-340s.txt`(开门前全 TCONF)、
  `target/oscomp/ltp-net-sctp-phase0-340s.txt`(开门后,accept_close 过)。

静态 case 合计 ≈ **~400**(含 9 个 `_v6` 变体)。当前确认通过:`accept_close`(10 case)。

## 阶段划分(初步,阶段 0 重跑后据实修正)

- **阶段 0**:`socket()` 支持 STREAM+SEQPACKET+IPPROTO_SCTP → `SocketKind::Sctp`;
  `modules.builtin` 加 sctp;重跑拿真实逐测试地形。
- **阶段 1**:1-to-1 生命周期 + ~20 sockopt + getname/多宿主地址(case 最密)。
- **阶段 2**:数据面 + 事件/通知模型(assoc_change)+ shutdown/abort。
- **阶段 3**:1-to-many(SEQPACKET)+ peeloff + bindx/connectx + fragments + IPv6 变体。

## 细表(41 条目)

| 条目 | case(静态) | 当前状态 | 目标阶段 | witness |
| --- | ---: | --- | --- | --- |
| `test_sockopt` | 44 | TCONF(门) | 1 | — |
| `test_sockopt_v6` | 44 | TCONF(门) | 1 | — |
| `test_1_to_1_sockopt` | 23 | TCONF(门) | 1 | — |
| `test_tcp_style` | 22 | TCONF(门) | 1 | — |
| `test_tcp_style_v6` | 22 | TCONF(门) | 1 | — |
| `test_1_to_1_socket_bind_listen` | 15 | TCONF(门) | 1 | — |
| `test_basic` | 15 | TCONF(门) | 1 | — |
| `test_basic_v6` | 15 | TCONF(门) | 1 | — |
| `test_getname` | 13 | TCONF(门) | 1 | — |
| `test_getname_v6` | 13 | TCONF(门) | 1 | — |
| `test_1_to_1_addrs` | 10 | TCONF(门) | 1 | — |
| `test_1_to_1_rtoinfo` | 3 | TCONF(门) | 1 | — |
| `test_1_to_1_initmsg_connect` | 2 | TCONF(门) | 1 | — |
| `test_inaddr_any` | 2 | TCONF(门) | 1 | — |
| `test_inaddr_any_v6` | 2 | TCONF(门) | 1 | — |
| `test_1_to_1_sendmsg` | 14 | TCONF(门) | 2 | — |
| `test_1_to_1_accept_close` | 10 | **pass** | 2 | `target/oscomp/ltp-net-sctp-phase0-340s.txt` |
| `test_1_to_1_connect` | 10 | TCONF(门) | 2 | — |
| `test_sctp_sendrecvmsg` | 10 | TCONF(门) | 2 | — |
| `test_sctp_sendrecvmsg_v6` | 10 | TCONF(门) | 2 | — |
| `test_1_to_1_send` | 9 | TCONF(门) | 2 | — |
| `test_1_to_1_recvmsg` | 8 | TCONF(门) | 2 | — |
| `test_1_to_1_recvfrom` | 7 | TCONF(门) | 2 | — |
| `test_1_to_1_shutdown` | 6 | TCONF(门) | 2 | — |
| `test_connect` | 5 | TCONF(门) | 2 | — |
| `test_1_to_1_nonblock` | 5 | TCONF(门) | 2 | — |
| `test_1_to_1_events` | 4 | TCONF(门) | 2 | — |
| `test_1_to_1_sendto` | 4 | TCONF(门) | 2 | — |
| `test_recvmsg` | 2 | TCONF(门) | 2 | — |
| `test_assoc_abort` | 1 | TCONF(门) | 2 | — |
| `test_assoc_shutdown` | 1 | TCONF(门) | 2 | — |
| `test_connectx` | 10 | TCONF(门) | 3 | — |
| `test_1_to_1_connectx` | 9 | TCONF(门) | 3 | — |
| `test_peeloff` | 6 | TCONF(门) | 3 | — |
| `test_peeloff_v6` | 6 | TCONF(门) | 3 | — |
| `test_timetolive` | 6 | TCONF(门) | 3 | — |
| `test_timetolive_v6` | 6 | TCONF(门) | 3 | — |
| `test_fragments` | 4 | TCONF(门) | 3 | — |
| `test_fragments_v6` | 4 | TCONF(门) | 3 | — |
| `test_autoclose` | 1 | TCONF(门) | 3 | — |
| `test_1_to_1_threads` | 1 | TCONF(门) | 3 | — |

状态图例:`TCONF(门)` = 卡在 `tst_check_driver` 驱动门;`pass`/`partial`/`fail` =
开门后的真实结果(阶段 0 重跑后填)。
