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

静态 case 合计 ≈ **~400**(含 9 个 `_v6` 变体)。当前确认通过:`accept_close`(10)、
`test_1_to_1_rtoinfo`(3)、`test_1_to_1_initmsg_connect`(2)、`test_1_to_1_sockopt`(22)、
`test_getname`(13)、`test_getname_v6`(13)、`test_1_to_1_socket_bind_listen`(14)、
`test_1_to_1_connect`(10)、`test_1_to_1_nonblock`(5)、`test_1_to_1_send`(8)、
`test_1_to_1_recvfrom`(7)、`test_1_to_1_sendto`(4)、`test_1_to_1_events`(4)、
`test_1_to_1_shutdown`(6)、`test_tcp_style`(22)、`test_tcp_style_v6`(22)、
`test_inaddr_any`(2)、`test_inaddr_any_v6`(2)、`test_recvmsg`(2)。

**阶段 3 — 1-to-many(SEQPACKET)模型开张(2026-06-05):** `test_inaddr_any(+v6)`、
`test_recvmsg` 过。模型:`RawSctpState` 加 per-socket 关联表(`peers: Vec<SctpAssoc>`)+
帧带 `source`;新 `step_send_sctp_seqpacket`:`sendmsg(msg_name)` 查 dst 上 bound/listening
的 socket,首次接触建联并给双方订阅端发 COMM_UP,数据**直投监听 socket 自身**(无 accept)
带 source;recvmsg 用 frame.source 填 msg_name;close 时给各 peer 发 SHUTDOWN_COMP
(`step_socket_close` 的 Bound/Listening 臂)。`sendmsg_impl` 按 sock_type 分流
(SeqPacket→seqpacket 路径,Stream→1-to-1)。**剩余 1-to-many**:test_basic(case6
SIGSEGV,疑 getladdrs)、test_connect(peeloff)、assoc_shutdown/abort(SCTP_STATUS
要反映已拆关联)、sctp_sendrecvmsg、test_sockopt(+v6)。

**阶段 2 tcp_style 收尾(2026-06-05):** `test_tcp_style(+v6)`(各 22)过。三处修正:
- connect 失败(accept 队列满 ECONNREFUSED)不再把 socket 卡在 Connected ——
  `step_sctp_connect` 改为**先入 accept 队列成功后**才置 Connected + COMM_UP,
  否则后续 connect 会误返 EISCONN。
- 已订阅 assoc_event 的 socket 做 `SHUT_WR` → 在本端入队 `SCTP_SHUTDOWN_COMP`
  通知(`step_shutdown`),排在待收数据之后。
- 1-to-1 socket 上 sendmsg 带 `SCTP_EOF`/`SCTP_ABORT`(sinfo_flags)→ `EINVAL`
  (在空 iov 早返前检查;`parse_sctp_sndrcvinfo` 现也读 sinfo_flags)。

**阶段 2 shutdown 语义(2026-06-05):** `test_1_to_1_shutdown`(6)过。
- `SHUT_WR`/`SHUT_RDWR` 在 1-to-1 SCTP 上**通知对端读侧 EOF**(`step_shutdown` 找对端
  fire `RecvWireSet::BROKEN`),对端 recv 无数据时返 0(EOF);本端 SHUT_WR 后 drained
  的 recv 返 ENOTCONN(复用 `sctp_recv_disconnected`)。
- 向**已 SHUT_RD 的对端**发送:接受并丢弃(返成功),不再 EPIPE(否则 flag=0 的 send 触发 SIGPIPE)。
- `shutdown` 未建联 SCTP socket → `ENOTCONN`(`socket_can_shutdown` 加状态检查)。

**阶段 2 事件模型(2026-06-05):** `test_1_to_1_events`(4)过。实现:
- `SCTP_EVENTS` 订阅(`sctp_event_subscribe`,存 `SctpLevelOptions.events_subscribe`)。
- 通知作为特殊"消息"入收队列(`SctpFrame { notification, stream, ppid, data }`),
  recvmsg 投递时置 `MSG_NOTIFICATION`。COMM_UP 在 connect 建联时入双方队列(若订阅
  assoc_event);SHUTDOWN_EVENT 在对端 close 时入本端队列(若订阅 shutdown_event)。
- `sctp_sndrcvinfo` cmsg round-trip:sendmsg 解析 SCTP_SNDRCV cmsg 的 stream/ppid 随
  消息存帧;recvmsg 为数据消息回填 SCTP_SNDRCV cmsg(stream/ppid)。
- 结构:`sctp_assoc_change`(20B)/`sctp_shutdown_event`(12B)字节构造在
  `net/execution/mod.rs`;`SocketRecvBytesOutcome` 加 `sctp_notification/stream/ppid`。

**阶段 2 续(2026-06-05,无事件):** `test_1_to_1_recvfrom`(7)+`test_1_to_1_sendto`(4)过。
- recvfrom:(a) recv 在未建联(listening/未连/SHUT_WR 后)的 SCTP socket 上返 `ENOTCONN`
  而非阻塞——`step_recv` 加 `sctp_recv_disconnected`,且因 recvfrom/recv 在到达 recv step
  前有 poll-wait 循环,shim `recvfrom_impl` 阻塞前也加同款检查;(b) recvfrom 用坏缓冲区
  (-1)要 EFAULT 且**不能吞掉已排队消息**——consume 前先 `validate_user_range(Write)`
  校验目的缓冲(之前是 consume 进 staging 后 copy-out 才失败,消息丢了)。
- sendto:对**未连接**的 1-to-1 SCTP socket 带目的地址 sendto → **隐式建联**
  (autobind + step_connect)再发(`sendto_impl`);已连接则忽略目的地址。

**纯阶段一(不碰数据面/事件)已见底**:`test_1_to_1_connect` 靠 3 处 connect 语义修正
通过——非法地址族→`EINVAL`(SCTP 把 read_sockaddr_in 的 EAFNOSUPPORT 映射为 EINVAL,
对标 Linux `sctp_verify_addr`)、connect on listening→`EISCONN`(`socket_can_connect`
+ `step_sctp_connect` 加 Listening 臂)、queue 满→`ECONNREFUSED`(backlog 修复)。
`test_1_to_1_nonblock` **不是纯阶段一**:TEST2/4 的非阻塞 connect→`EINPROGRESS`、
recvmsg→`EAGAIN` 可做,但 TEST5 要 `sendmsg`/`recvmsg`+`sctp_sndrcvinfo`+`MSG_EOR`
(数据面),全过得等阶段 2。其余未过条目均需阶段 2(数据面+`SCTP_EVENTS` 通知)或
阶段 3(`sctp_getladdrs` 多宿主地址)。

**阶段 2 进展(数据面 MVP,2026-06-04)**:实现了消息边界保留的 SCTP recv
(`RawSctpState::recv_message` 一次取一条 front 消息)+ `MSG_EOR`(经
`SocketRecvBytesOutcome.eor` 透传到 recvmsg `msg_flags`)+ 非阻塞 SCTP connect 返
`EINPROGRESS`(loopback 同步建联但对标 Linux 语义)。**新过:`test_1_to_1_nonblock`(5)、
`test_1_to_1_send`(8)。** 数据面探针其余结果:`sendto` 2(断 case3 sendto-from)、
`recvmsg` 3 后 **SIGSEGV(139)**、`recvfrom` 3 后超时(case4 阻塞等数据)、
`sendmsg` 4(断 case5)、`sctp_sendrecvmsg` 0(首条即需事件/setup)。后续:
`sctp_sndrcvinfo` cmsg 真正 round-trip(stream/ppid/assoc_id)、`SCTP_EVENTS` 通知模型。

> **`test_1_to_1_recvmsg` 的 SIGSEGV 是 musl 不兼容,不是内核 bug(已查实)**:case4
> `recvmsg(acpt_sk, (struct msghdr *)-1, flag)` 期望 EFAULT,但 musl 的 recvmsg
> wrapper 在 64 位上会先 `h = *msg`(把 msghdr 拷到栈上以修正 iovlen/controllen 字段
> 宽度),于是在**进内核前**就在用户态解引用 -1 → 段错误(user-segv pc 在 libc,
> 非内核;case3 用合法 msghdr+坏 iov 字段则正常返 EFAULT)。lksctp 测试是按 glibc 写的,
> 这是 glibc/musl 差异,内核侧无法修(改测试/libc 属掩盖,禁止)。
> 内核的 `copy_to/from_user(-1)` 本身已正确返 EFAULT(recvfrom case3 即证),无需加固。
> → `test_1_to_1_recvmsg` 在 musl 下最多过 3/8。

**phase-1 探针(2026-06-04,逐个单跑)发现的剩余 blocker**:
- `test_basic(+v6)`:socket/bind 过后断在 `setsockopt(SCTP_EVENTS)` →"Protocol not
  available"(catch-all 返 ENOPROTOOPT)。但即便实现 EVENTS,test_basic 还要 sendmsg/
  recvmsg + COMM_UP/SHUTDOWN 通知 + `sctp_getladdrs/getpaddrs` —— 整体属**阶段 2-3**。
- `test_inaddr_any(+v6)`:首条就是 `SCTP_EVENTS` + 通知路径,同上(阶段 2)。
- `test_1_to_1_addrs`:前 3 个 errno 边界 case 过(含 EOPNOTSUPP),断在
  `sctp_getladdrs` 取真实本地地址列表(返 EOPNOTSUPP)——需多宿主地址 API(阶段 3)。
- `test_tcp_style(+v6)`:**根因已查明并修掉一半**。原断点不是"第一个 connect",而是
  **第 10 个**:测试 `listen(MAX_CLIENTS-1)=listen(9)` 后连 `MAX_CLIENTS=10` 个客户端,
  期望全成功(再第 11 个 `clt2` 才被拒)。我们的 accept 队列 `is_full()` 用 `>= limit`
  只收 9 个 → 第 10 个 `ETIMEDOUT`。Linux 语义是 `sk_ack_backlog > sk_max_ack_backlog`,
  即 listen(N) 收 **N+1** 个。已把 `TcpBacklog::is_full` 和 `SocketAcceptQueue::push`
  改为 `> limit`(payload.rs),TCP/SCTP 通用。**结果:tcp_style 2→10 TPASS**。
  顺带修了 `step_bind` 的潜伏 bug:autobind 在已 bound/listening 的 socket 上调用
  step_bind 会在协议状态检查拒绝(EINVAL)前就改了 bind 索引且不回滚 —— 现改为先查
  可绑定状态再动表。两处都加了 host 回归测试(`rds_sctp_ltp_tests.rs`)。
  **剩余 blocker(阶段 2-3)**:case 11 `recv(listen_sk)` 在未连接 socket 上应即时返错
  却阻塞;case 12+ 要真实数据收发;case 13 `recv SHUTDOWN_COMP notification` 要
  `SCTP_EVENTS` 订阅 + assoc_change/shutdown 通知。所以现在 case 11 会卡到超时,
  整测试仍 FAIL —— 要等阶段 2 数据面+通知模型。

**阶段 1 进展**:`SctpLevelOptions` sockopt 存储 + `SOL_SCTP` 接线已覆盖
`SCTP_RTOINFO`/`SCTP_INITMSG`/`SCTP_ASSOCINFO`/`SCTP_STATUS`/`SCTP_PRIMARY_ADDR`/
`SCTP_AUTOCLOSE`(`socket.rs`)。要点:
- `SCTP_AUTOCLOSE` 在 1-to-1(STREAM)socket 上按 Linux 返回 `EOPNOTSUPP`
  (autoclose 仅对 1-to-many 有意义);未知 SCTP optname 的 setsockopt 走 catch-all
  返回 `ENOPROTOOPT`,getsockopt 走 catch-all 返回 `EOPNOTSUPP`。
- `SO_SNDBUF`/`SO_RCVBUF` 现按 Linux 语义存 **2×** 请求值(下限 `SOCK_MIN_BUF`),
  getsockopt 读回翻倍值——这是全局行为(非仅 SCTP),`test_1_to_1_sockopt` TEST14/16/17/18 依赖。
- `SCTP_STATUS`/`SCTP_PRIMARY_ADDR` 用 loopback 对端 endpoint 合成只读结构
  (无真实多宿主/路径度量);`SCTP_ASSOCINFO` 全字段 round-trip 存储。
后续 sockopt(EVENTS/PEER_ADDR_PARAMS/…)照此模式叠加。
注意 `test_sockopt`(最密 88 case)还需 1-to-many sendmsg/recvmsg + 事件,属阶段 2-3。

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
| `test_1_to_1_sockopt` | 23 | **pass** | 1 | `target/oscomp/ltp-net-sctp-1to1-sockopt.txt` |
| `test_tcp_style` | 22 | **pass** | 2 | `target/oscomp/ltp-net-sctp-test_tcp_style.txt` |
| `test_tcp_style_v6` | 22 | **pass** | 2 | `target/oscomp/ltp-net-sctp-test_tcp_style_v6.txt` |
| `test_1_to_1_socket_bind_listen` | 15 | **pass** | 1 | `target/oscomp/ltp-net-sctp-test_1_to_1_socket_bind_listen.txt` |
| `test_basic` | 15 | TCONF(门) | 1 | — |
| `test_basic_v6` | 15 | TCONF(门) | 1 | — |
| `test_getname` | 13 | **pass** | 1 | `target/oscomp/ltp-net-sctp-getname.txt` |
| `test_getname_v6` | 13 | **pass** | 1 | `target/oscomp/ltp-net-sctp-getname-v6.txt` |
| `test_1_to_1_addrs` | 10 | TCONF(门) | 1 | — |
| `test_1_to_1_rtoinfo` | 3 | **pass** | 1 | `target/oscomp/ltp-net-sctp-rtoinfo-120s.txt` |
| `test_1_to_1_initmsg_connect` | 2 | **pass** | 1 | `target/oscomp/ltp-net-sctp-1to1-initmsg.txt` |
| `test_inaddr_any` | 2 | **pass** | 2 | `target/oscomp/ltp-net-sctp-m-test_inaddr_any.txt` |
| `test_inaddr_any_v6` | 2 | **pass** | 2 | `target/oscomp/ltp-net-sctp-m-test_inaddr_any_v6.txt` |
| `test_1_to_1_sendmsg` | 14 | TCONF(门) | 2 | — |
| `test_1_to_1_accept_close` | 10 | **pass** | 2 | `target/oscomp/ltp-net-sctp-phase0-340s.txt` |
| `test_1_to_1_connect` | 10 | **pass** | 1 | `target/oscomp/ltp-net-sctp-test_1_to_1_connect.txt` |
| `test_sctp_sendrecvmsg` | 10 | TCONF(门) | 2 | — |
| `test_sctp_sendrecvmsg_v6` | 10 | TCONF(门) | 2 | — |
| `test_1_to_1_send` | 9 | **pass** | 2 | `target/oscomp/ltp-net-sctp-test_1_to_1_send.txt` |
| `test_1_to_1_recvmsg` | 8 | musl-blocked 3/8 | 2 | `target/oscomp/ltp-net-sctp-test_1_to_1_recvmsg.txt` |
| `test_1_to_1_recvfrom` | 7 | **pass** | 2 | `target/oscomp/ltp-net-sctp-test_1_to_1_recvfrom.txt` |
| `test_1_to_1_shutdown` | 6 | **pass** | 2 | `target/oscomp/ltp-net-sctp-test_1_to_1_shutdown.txt` |
| `test_connect` | 5 | TCONF(门) | 2 | — |
| `test_1_to_1_nonblock` | 5 | **pass** | 2 | `target/oscomp/ltp-net-sctp-test_1_to_1_nonblock.txt` |
| `test_1_to_1_events` | 4 | **pass** | 2 | `target/oscomp/ltp-net-sctp-test_1_to_1_events.txt` |
| `test_1_to_1_sendto` | 4 | **pass** | 2 | `target/oscomp/ltp-net-sctp-test_1_to_1_sendto.txt` |
| `test_recvmsg` | 2 | **pass** | 2 | `target/oscomp/ltp-net-sctp-m-test_recvmsg.txt` |
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
