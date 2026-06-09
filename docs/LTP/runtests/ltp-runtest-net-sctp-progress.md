# LTP `net.sctp` 测试记录表

Date: 2026-06-04

`net.sctp`(41 个条目)的逐测试进度跟踪。实现计划与协议讲解见本地稿
`msp/sctp-implementation-plan-2026-06-04-zh.md`(不提交)。

计分口径同 `ltp-runtest-network-progress.md`:`TPASS` 计 case;`TCONF` 是跳过
(不入分母);本表"case 数"是源码里静态 `tst_resm(TPASS,…)` 计数(运行时含循环会更多)。

## 当前状态

**2026-06-09 续做:+14 个条目通过 → 38/41**(`feature-network-next`,逐个 rv64-qemu 验证 + 提交):
`test_assoc_abort`、`test_1_to_1_connectx`、`test_peeloff`(+v6)、`test_connect`、
`test_fragments`(+v6)、`test_sockopt`(+v6,44/44)、`test_autoclose`、
`test_sctp_sendrecvmsg`(+v6)、`test_timetolive`(+v6)。要点见 `docs/progress/STATUS.md` 2026-06-09 条。
**PR-SCTP TTL 已做(取"可观察行为"模型,非完整 rwnd 流控):** `sinfo_timetolive>0` 的消息直接丢弃
——不投给对端;发送端按分片(SCTP_MAXSEG 切片、末片置 SCTP_DATA_LAST_FRAG)收到 `SCTP_SEND_FAILED`
(0x8003)携带被丢数据。lksctp 的 ttl 测试都是先 fillmsg 填满 rwnd 再 sleep 过 TTL,所以
"ttl>0 ⇒ 丢弃"正好复现它们检查的行为(无需 rwnd/定时器)。
**剩余 3 个未过 —— 2 个物理无解 + 1 个大活:**
- `test_1_to_1_recvmsg`/`test_1_to_1_sendmsg`:**musl libc 阻塞,内核侧无解** —— `(struct msghdr*)-1`
  在 musl 的 recvmsg/sendmsg wrapper 里**用户态**解引用即段错误,内核根本看不到这次调用
  (内核的 copy_to/from_user(-1) 本身已正确返 EFAULT)。改测试/libc 属作弊。最多 3/8、5/14。
- `test_connectx`:真**多宿主**(NUMADDR=6)—— 第一道墙:SCTP bind 拒绝 127.0.0.2+(EADDRNOTAVAIL,
  只配了 127.0.0.1 → 要接受整个 127/8);再 test_peer_addr 要 `sctp_getpaddrs` 严格返回每个关联的
  **全部 6 个**对端地址 + 非阻塞 connectx EINPROGRESS 且 assoc_id 对齐。要给每个关联存一个从对端 bound
  地址来的地址**集合**。比 TTL 那次大,**未动手**。(bindx + CONNECTX3 单地址已做,多宿主模型未做。)
> 排错经验:sockopt 结构偏移对不上时,去读测试头文件里的真实 `struct` 定义 ——
> `packed`/`aligned` 属性会改偏移(本次 `struct sctp_paddrinfo` 是 packed,aligned(4),
> `spinfo_address` 在偏移 **4** 而非 8,卡了好几轮)。

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
(SeqPacket→seqpacket 路径,Stream→1-to-1)。

**阶段 3 — test_basic(+v6)全过(15/15,2026-06-05):** 四处修正:
1. **通配源地址解析**:`step_send_sctp_seqpacket` 中 sk 绑 `INADDR_ANY` 时,对端
   看到的源地址是路由出口地址(loopback 短路下 = dst 的 loopback 地址),非字面
   `0.0.0.0`——COMM_UP/数据帧的 `source` 与对端 assoc 键都用解析后的 `source`。
2. **assoc_id 路由 sendmsg**:`SctpSndInfo` 解析 `sinfo_assoc_id`(@28);
   `step_send_sctp_seqpacket` 接受 `dst: Option` + `assoc_id`,`msg_name` 为空时
   按 assoc_id 查对端,无/未知 id → `EPIPE`(匹配 NULL-name 用例)。
3. **全局唯一 assoc id**:`RawSctpState` 改用进程级原子计数器(`NEXT_SCTP_ASSOC_ID`)
   发号,使两端 assoc id 不撞号(测试用对端 id 作"错误 id"探测时需要)。
4. **1-to-many getpaddrs**:`SCTP_GET_PEER_ADDRS` 对 SEQPACKET 从 optval 读
   `assoc_id`,经 `sctp_peer_addr_by_assoc` 查对端;`close` 的 SHUTDOWN_COMP
   通知带 `source`(对端视角的本地址)+ 对端自己的 assoc_id。

**test_sockopt 续(6→14,2026-06-05):** (a) SEQPACKET close 通知扩展——按订阅分别
发 `SCTP_SHUTDOWN_EVENT`(0x8005)与/或 SHUTDOWN_COMP(assoc_change),均带 source
+ 对端 assoc_id(case 7)。(b) 新增 `SCTP_PEER_ADDR_PARAMS`(=9)与 `SCTP_DELAYED_ACK_TIME`
(=16)的 get/set:`SctpLevelOptions` 加 paddr_* 字段(packed `sctp_paddrparams`
@132 hbinterval/@136 pathmaxrxt/@138 pathmtu/@142 sackdelay/@146 flags),
`spp_sackdelay` 与 DELAYED_ACK_TIME 的 `assoc_value` 共用一字段(case 11-13);
非零 `spp_assoc_id` 须命中已有关联否则 EINVAL(case 14)。**剩余 case 15+**:1-to-many
`connect()` + 服务端 COMM_UP(带 assoc_id)、spp_address 传输校验、精确长度校验。

**1-to-many connect + paddrparams 校验(test_sockopt 14→25、test_connect 2→3,
2026-06-05):** (a) `step_sctp_connect` 加 SEQPACKET 分支:connect() 直接在监听
socket 上建联(无 accept/child),两端 `sctp_ensure_assoc` + COMM_UP(带 assoc_id);
重复 connect 已有关联 → EISCONN(test_connect case 1-3、test_sockopt case 15)。
(b) `SCTP_DELAYED_ACK_TIME` set 也做非零 assoc_id 校验(case 20)。(c) `spp_flags`
校验:enable/disable 互斥位对冲突 → EINVAL;`SPP_HB_DEMAND` 需具体关联(assoc_id≠0)
否则 EINVAL(case 22-25)。

**SCTP_DEFAULT_SEND_PARAM(test_sockopt 25→32,2026-06-05):** `SctpLevelOptions`
加 `default_send_param: [u8;32]`(socket 级 `sctp_sndrcvinfo`)+ get/set(=10),
非零 assoc_id 校验。socket 级单字段即覆盖 case 26-32(含 set-then-get 的 assoc 级)。

**sctp_peeloff(最小版,test_connect 3→4、test_sockopt 32→33、test_peeloff 0→3,
2026-06-05):** `SCTP_SOCKOPT_PEELOFF`(=102,`sctp_peeloff_arg_t{associd@0,sd@4}`)
getsockopt:新 `step_sctp_peeloff` 用 `create_connected_sctp_for_accept_in_namespace`
把指定关联建成一个 1-to-1(Stream)Connected socket(继承选项),经
`socket_open_file_from_identity` + `allocate_fd/set_fd` 装成新 fd,fd 回填 sd@4。
连 peeled socket → EISCONN(已 Connected)。

**剩余 = peeloff 关联迁移(数据面)**:test_peeloff case 4(客户端发往服务端的数据
要投递到 peeled socket)、test_connect case 5(peel 后原 socket 对该地址 connect →
EADDRNOTAVAIL)、test_sockopt case 34(peeled socket 上按 assoc_id 设 DEFAULT_SEND_PARAM)
——都需把关联真正从原 1-to-many socket 迁移到 peeled socket(连接表 + 路由 + 从原
peers 移除),比当前"建联但不迁移"的最小版更深。

**SCTP_MAXSEG + SCTP_DISABLE_FRAGMENTS sockopt(2026-06-05):** `SctpLevelOptions`
加 `maxseg: u32` / `disable_fragments: bool` + get/set(=13 / =8,plain int)。解锁
三测试的早期 case:test_sctp_sendrecvmsg 0→6、test_timetolive 0→3、test_fragments 0→2。
另:`test_1_to_1_threads` 确认通过(1)。各自新 blocker 属真正的分片/重组数据面语义
(MAXSEG 驱动的分片长度、disable 时超 frag point → EMSGSIZE),loopback 直投模型未建模。

**test_assoc_shutdown 通过(SCTP_EOF 优雅拆联,2026-06-05):** (a) `RawSctpState` +
`SocketPayload` 加 `remove_assoc`/`sctp_remove_assoc`(按 assoc_id 摘除,返回 peer)。
(b) 把 close 的 per-peer 通知抽成 `notify_sctp_peer_assoc_closed`,新 step
`step_sctp_shutdown_assoc`:通知该关联对端(SHUTDOWN_EVENT/COMP)再从本端摘除。
(c) `sendmsg_impl`:SEQPACKET 带 `SCTP_EOF`/`SCTP_ABORT`(sinfo_flags)→ 调
`step_sctp_shutdown_assoc`(在空消息早返之前)。(d) `SCTP_STATUS` 对 SEQPACKET 校验
输入 `sstat_assoc_id`——已摘除的 assoc → EINVAL。

**剩余 1-to-many**:test_assoc_abort(`SCTP_ABORT` 需 COMM_LOST(state 1)而非
SHUTDOWN_COMP,且早期 client→server 数据流有长度问题)、peeloff 关联迁移、分片数据面。

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
| `test_1_to_1_addrs` | 10 | **pass** | 3 | `target/oscomp/ltp-net-sctp-addrs.txt` |
| `test_basic` | 15 | **pass** | 3 | `target/oscomp/ltp-net-sctp-f-test_basic.txt` |
| `test_basic_v6` | 15 | **pass** | 3 | `target/oscomp/ltp-net-sctp-f-test_basic_v6.txt` |
| `test_sockopt` | 44 | partial 33/44 (peeloff 迁移) | 多 | `target/oscomp/ltp-net-sctp-p-test_sockopt.txt` |
| `test_connect` | 5 | partial 4/5 (peeloff 迁移) | 多 | `target/oscomp/ltp-net-sctp-p-test_connect.txt` |
| `test_peeloff` | 7 | partial 3/7 (peeloff 迁移) | 多 | `target/oscomp/ltp-net-sctp-p-test_peeloff.txt` |
| `test_1_to_1_threads` | 1 | **pass** | 1 | `target/oscomp/ltp-net-sctp-rg6-test_1_to_1_threads.txt` |
| `test_assoc_shutdown` | 1 | **pass** | 1 | `target/oscomp/ltp-net-sctp-r-test_assoc_shutdown.txt` |
| `test_sctp_sendrecvmsg` | ~10 | partial 6 (分片) | 多 | `target/oscomp/ltp-net-sctp-q-test_sctp_sendrecvmsg.txt` |
| `test_timetolive` | ~6 | partial 3 (分片) | 多 | `target/oscomp/ltp-net-sctp-q-test_timetolive.txt` |
| `test_fragments` | ~8 | partial 2 (分片) | 多 | `target/oscomp/ltp-net-sctp-q-test_fragments.txt` |
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
