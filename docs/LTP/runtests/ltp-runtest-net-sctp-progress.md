# LTP `net.sctp` 测试记录表

Date: 2026-06-04

`net.sctp`(41 个条目)的逐测试进度跟踪。实现计划与协议讲解见本地稿
`msp/sctp-implementation-plan-2026-06-04-zh.md`(不提交)。

计分口径同 `ltp-runtest-network-progress.md`:`TPASS` 计 case;`TCONF` 是跳过
(不入分母);本表"case 数"是源码里静态 `tst_resm(TPASS,…)` 计数(运行时含循环会更多)。

## 当前状态

- **0 / 41 通过**(2026-06-04 probe)。
- 41 个二进制**都在镜像里、都能跑**,但**全部 TCONF 在 `tst_check_driver("sctp")`
  这道门**——连各自的 `socket()` 都没走到。
- 门机制:`tst_check_driver`(`lib/tst_kernel.c`)读 `/lib/modules/$(uname -r)/modules.builtin`
  和 `modules.dep`,找 "sctp"。现在没有 → 判"驱动不可用"。
- ⚠️ 门必须**和真实 SCTP 实现一起开**:单开门会把 TCONF 变成 FAIL/TBROK(更难看)。
- witness:`target/oscomp/ltp-net-sctp-probe-340s.txt`(全 TCONF)。

静态 case 合计 ≈ **~400**(含 9 个 `_v6` 变体)。

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
| `test_1_to_1_accept_close` | 10 | TCONF(门) | 2 | — |
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
