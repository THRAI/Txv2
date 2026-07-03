# P3 执行计划：上半接入 —— socket 成一等文件对象 + 等待统一

<!-- txdoc:07-NET-P3-V1 -->

> 阶段来源：[`REFACTOR_PLAN_A_v2.md`](REFACTOR_PLAN_A_v2.md) §5-P3。v2 的 P3 包含三块：**①上半接入**（D13 FileOps + D14 wait 收敛/R4a）、**②模型瘦身**（9×Option→enum、D4 单锁、D7 就绪单一来源）、**③R 族修复**（R1b-e 并发、R2b/c/d/e/f 资源）。**本文档 = P3-A（上半接入）的执行计划**；②③分别作为 P3-B/P3-C 在 P3-A 落地后另立执行文档——三块相对独立、行为面各异，一次全铺会让回滚单元和回归判定失焦（P1/P2 的每步独立 commit 纪律无法维持）。
>
> 取证：两路并行调查（D13 文件抽象 / D14 等待收敛）+ orchestrator 亲验 VFS 分发臂与 CharDeviceOps 样板。所有 file:line 按 `c643c8e4`。

---

## 1. 病根与目标（审计⑤⑥/R4a）

**⑤ 无文件系统接口**：socket **已经是** fd 表一等公民——`Cap<OpenFile>` → `OpenFileBacking::Rnode` → `RNodeBacking::StructBacked` → `StructPayload::Socket { identity: Cap<SocketIdentity> }`（vfs/structure.rs:560-563），与字符设备同壳。但同一个 `step_read` match 里，`CharDevice(binding)` 臂走 `binding.ops.read(...)` 多态（vfs/execution.rs:382），`Socket {..}` 臂却选 `EINVAL`（vfs/execution.rs:396-399，write 侧 649-652）——把 socket I/O 逼到 syscall 层十几处 `socket_identity_from_file(...).is_ok()` 特判（清单见 §2）。目标 = Linux `socket_file_ops` / asterinas `Socket: FileLike`：引入与 `CharDeviceOps`（device.rs:47-50）同形但更宽的 **FileOps** trait，VFS 臂委派、特判清零。

**⑥/R4a 等待分裂**：全栈存在**两套互不连通的 wait 注册表**——subsystems 侧 `REGISTRY`（wait_source.rs:60，收 Channel/RawQueue/RawPort，`wait_on_token` 消费）与 substrate 侧 `REGISTRY`（tx-substrate wake/wait_source.rs:444，收 `Arc<WaitSource>`，`await_wait_source`/`lookup_source` 消费）。socket 的三个就绪队列 `SocketReadiness{recv_wq,send_wq,accept_wq}`（readiness.rs:30-33）只登记在前者（identity.rs:90-98）；epoll 只查后者。**R4a 精确行为**：`epoll_ctl ADD` 存的是 subsystems carrier id（epoll.rs:117-126→489-490），`epoll_wait` 的 `await_any_wait_source` 逐个 `lookup_source` 查 substrate 表查不到 → 跳过（wait.rs:48-57）→ 纯 socket 集合 `active.is_empty()` → **立即 `Return(0)`**（epoll.rs:576-577）——即使 timeout=-1。应用层退化为忙轮询；混合集合则只能搭其他 fd 的唤醒便车。ppoll/pselect 因走 `wait_on_token`（subsystems 表）而幸免（io.rs:1286-1330）。此外 socket 阻塞路径是全栈唯一的 legacy 形态：`wait_on_yield_shape`（helpers.rs:1714-1723 重新打包 WaitToken）+ 多处 `yield_now()` 忙让步（socket.rs:893/899/908/1092/1168）。

**现成范式（判决性发现）**：eventfd 已实现"同一逻辑 source 双表可见"——`allocate_notification_source_id()`（1<<32 起，lib.rs:44-48）→ `new_wait_source(id)` 登 substrate 表（eventfd/adapter.rs:73-77）→ `register_wait_channel_with_id(id, ...)` 同 id 登 subsystems 表（eventfd/notification.rs:39-56）→ 状态跃迁双通知（notification.rs:58-73）。epoll 因此能阻塞在 eventfd 上。**socket 缺的恰是第 2、4 步**。pipe/timerfd/signalfd/futex/aio 同款。

---

## 2. 特判清单（S 步的"完成度量尺"）

| 位置 | 形态 | 归宿 |
| --- | --- | --- |
| io.rs:1946-1948 | `sys_write` if socket → `sys_sendto` | S3 删（走通用 step_write→ops） |
| io.rs:2229-2245 | `sys_read` if socket → poll-gate + `sys_recvfrom` | S3 删（同上） |
| io.rs:1073/1100/1124/1144 | ppoll socketpair/socket mask+token | S4 统一 poll 接口 |
| io.rs:1468/1497-1533 | pselect 同上 | S4 |
| epoll.rs:119/188/255 | epoll 特判 mask/token/eligibility | S4（S1 已使其可阻塞） |
| fs_basic.rs:1516-1520 | ioctl if Socket → `sys_socket_ioctl` | S5 |
| fs_basic.rs:419-427 | fcntl F_SETFL 后 socket 专属 fire_send | S5 |
| fs_basic.rs:1176-1204/667 | close/close_range 后 `maybe_close_socket_file_after_fd_remove` | S5（评估后可保留为 ops.close 委派） |
| splice.rs:69-72/87-90 | splice 拒 socket EINVAL | **保留**（拒绝语义本身合法，splice-socket 支持非本期目标） |
| process/execution.rs:1009-1039 | 进程退出批量 close socket 专用车道 | S5 评估收编 |

另：`linux_syscall/net.rs`（FakeSocket，net.rs:26-36）为死代码（`mod net` 未声明）——S6 删除。

---

## 3. 分步实施（S1–S6，每步独立编译/验证/提交/可回滚）

### S1 —— R4a 判决修复：socket 就绪载体双表可见（D14a，最小闭环）

- 照抄 eventfd 范式：`SocketWaitCarriers::register`（identity.rs:90-98）改为每个载体 `allocate_notification_source_id()` 取 id → `register_wait_channel_with_id`/queue 同 id 登 subsystems 表（保 ppoll/pselect 与现有 socket park 不动）→ 同 id `new_wait_source`+`register_source` 登 substrate 表；`SocketReadiness::fire_*`（readiness.rs:46-66）双通知（RawQueue.fire + substrate WaitSource.notify）。urgent RawPort 同构。
- **判决单测**：host——epoll_ctl ADD 一个未就绪 socket → epoll_wait 的 wait-source 收集非空（不再被 lookup_source 跳过）；fire recv → 唤醒。**QEMU 见证**：新增 `epoll-external-smoke.c`（epoll_create1 + ADD 外部 connect 的 socket + epoll_wait 阻塞等响应 → EPOLLIN → read → ok）——修前该冒烟必然忙轮询/超时，修后真阻塞真唤醒。
- 回滚单元：identity.rs/readiness.rs 两文件 + 冒烟。

### S2 —— FileOps trait 定义 + VFS 臂委派（D13a）

- 在 vfs（device.rs 旁）定义 `FileOps`（比 CharDeviceOps 宽）：`read/write`（签名同 CharDeviceOps，StepOutcome<usize, ByteProgress>）、`poll_mask(&Guard)->PollMask`、`poll_wait_token(interest)->Option<WaitToken 或 substrate id>`、`ioctl`、`on_set_fl`、`close`。socket 侧 `SocketFileOps`（持 `Cap<SocketIdentity>` 或按 identity 实现）实现之——**read/write 直接调 net 既有 step（step_recv/step_send 族）**，不绕道 syscall 层。
- `step_read`/`step_write` 的 `Socket{..}` 臂从 `EINVAL` 改 `ops.read/write` 委派（vfs/execution.rs:396-399/649-652）。本步 **不动** syscall 层特判（io.rs 转发仍在）——先让通用路径"能走"，特判"还在挡"，行为零变化，纯增量。
- 验证：host 单测直接调 `OpenFile::step_read/step_write` 于 socket-backed file，断言不再 EINVAL 而是与 step_recv/step_send 同语义；全冒烟矩阵不退化。

### S3 —— read/write 特判摘除（D13b，行为面最大步）

- 删 io.rs:1946-1948/2229-2245 转发，socket read/write 走通用 `sys_read/sys_write` → `step_read/step_write` → ops。readv/writev/pread/pwrite 家族自动继承。
- **语义对齐清单（灵魂验证点）**：`read(fd)` ≡ `recv(fd,buf,len,0)`、`write` ≡ `send(...,0)`；阻塞语义 = 通用路径对 Yield shape 的 await 必须与原 recvfrom 路径等价——特别是 **EINTR/itimer 唤醒**（原路径 `wait_on_socket_or_itimer`）与**送后 loopback drive/yield**（原 sendto 收尾 `finish_sendto_progress`——归入 ops.write 的完成钩或通用路径等价物；P2-S6 的 loopback-dst 谓词 gate 必须保持）。若通用 await 缺 itimer 耦合，本步补到通用层（使 pipe/tty 同受益）而非留特判。
- 验证：QEMU 全冒烟矩阵（ext/tcp-lo/udp-lo/dns/seq/bulk/accept 全部用 read/write 系 syscall，天然是回归见证）+ host 集合差 + 阻塞语义单测（recv 无数据阻塞、对端写唤醒、itimer 打断返回 EINTR）。
- 回滚单元：单 commit revert 即回退到 S2 状态（转发恢复）。

### S4 —— poll 家族统一（D13c + R4a 收口）

- `poll_mask`/`poll_wait_token` 进 ops；io.rs ppoll/pselect 与 epoll.rs 的 socket 特判改调统一接口（socketpair 分支一并收编或明确留账）。epoll 侧配合 S1 的 substrate id，socket 与 eventfd/timerfd 走完全同构的注册/等待/采样三步。
- 验证：epoll 冒烟（S1 的）+ ppoll/pselect 回归（LTP poll 族本机不可跑，以 host 单测覆盖：socket 未就绪 ppoll 阻塞→fire 唤醒→mask 正确）。

### S5 —— ioctl / F_SETFL / close 收编（D13d）

- ioctl：fs_basic.rs:1516 特判改 `ops.ioctl` 委派（实现即调 `sys_socket_ioctl` 内核侧等价 step）。
- F_SETFL：fs_basic.rs:419-427 的 socket 专属 fire_send 移入 `ops.on_set_fl`。
- close：评估 `maybe_close_socket_file_after_fd_remove`（socket.rs:3714-3728）与进程退出车道（process/execution.rs:1009-1039）收编为 `ops.close` 钩子——**保守裁量**：若 retain_count/两相时序有隐坑则保留 bolt-on 形态、只把"是不是 socket"的判定换成 ops 存在性，记账 P3-B。
- 验证：sockopt/ioctl 冒烟（FIONBIO/FIONREAD 单测）+ close 后端口释放回归（复用 P2 seq 冒烟——连续连接依赖 close 正确释放）。

### S6 —— D14b 等待收敛 + 清扫 ◐（2026-07-03 部分完成 + 范围修订）

> **实施记录**：死代码清扫完成——`linux_syscall/net.rs`（FakeSocket，401 行，`mod net` 从未声明）删除。**范围修订（诚实裁量）**：D14b 的 park 机械替换（`wait_on_yield_shape`/`wait_on_token` → `await_wait_source`）与 `yield_now` 忙让步清理**移交 LTP 环境轮**——两者的回归形态是 EINTR 语义与调度时序（recv 族/ping 族/poll 族 LTP），本机冒烟矩阵对它们不敏感（换不换机械都绿），没有裁判的重构违反本重构的判决性纪律；且忙让步可能掩护着潜在丢唤醒（P2 坑 5 家族），摘除必须有 LTP 兜底。S1 的双表登记已把收敛的地基打好（同 id 两表可见），届时为机械替换。
> **P3-A 收官状态**：S1-S5 全落地 + S6 清扫部分；特判清单（§2）核销情况——io.rs read/write ✅、poll/epoll 换轨 FileOps ✅、F_SETFL ✅、close/exit 车道 ✅、ioctl **保留**（需 ctx/用户内存，Step 形 trait 无法承载，记账 P3-B）、splice 拒绝**保留**（语义合法）、socketpair **保留**（独立形态，拍板 #4）、D14b park 收敛**移交 LTP 轮**。

- socket park 从 `wait_on_yield_shape`/`wait_on_token` 收敛 `await_wait_source`（S1 后 substrate id 已就位，机械替换）；清 `yield_now` 忙让步——**逐个裁量**：纯重试型让步删除；服务 loopback 内联 drive 公平性的语义性 yield（sendto 后让对端跑）保留并注释成契约。
- 删死代码 `linux_syscall/net.rs`；特判清单（§2）逐项核销；`wait_on_yield_shape` 若无余户则删。
- 验证：全矩阵 + 集合差 + busybox-boot + la64 构建收官。

---

## 4. 测试方案

1. **判决单测**：S1 epoll-blocks-on-socket（host）；S3 read≡recv/write≡send 语义对（host）；S4 ppoll 阻塞唤醒（host）。
2. **QEMU 矩阵**（单核，全部既有 + 新增）：ext/tcp-lo/udp-lo/dns/seq/bulk/accept + **epoll-external-smoke**（S1 新增，P3 验收载体）。
3. **不退化门**：tx-subsystems --lib 集合差（基线 308 + 已知新测试名清单——毒锁级联区新测试必显 NEW，判据以单跑+QEMU 为准）；`cargo -q xtask unit`（仅既有 ext4 失败）；busybox-boot；la64 full-build。
4. **暂缺**：LTP net 全量（本机无镜像）——poll/epoll/recv 族 LTP 在有环境时补跑，记入 STATUS。

---

## 5. 风险与已知坑

1. **S3 是行为面最大步**：read/write 阻塞语义从 socket 专用 await 循环换到通用循环，EINTR/itimer/非阻塞 EAGAIN/部分写语义都可能有细差——灵魂单测先行，QEMU 冒烟矩阵全部天然走此路径，回归立现。
2. **双表 fire 的时序**：S1 双通知需与 P1-S4 的 EBR 纪律兼容（fire 在 guard 内、禁嵌套 guard——`borrow_current_guard` 先例照用）。
3. **close 两相**：retain_count>1 的 dup/fork 场景与进程退出车道是手工维护的隐契约，S5 保守裁量、宁留 bolt-on 不抢收编。
4. **性能**：ops 动态分派每 I/O 一次虚调用，对 TCG 环境无感（对比 syscall 开销三个量级差）。
5. **回滚单元 = S 步**；S3 独立 revert 不影响 S1/S2。

---

## 6. 设计点拍板（按既定授权取推荐）

| # | 问题 | 取向 |
| - | --- | --- |
| 1 | FileOps 放哪 | **vfs 层（device.rs 旁）**，trait 对 net 零依赖；socket 实现放 net 侧，经 StructPayload::Socket 臂接线 |
| 2 | 双表可见 vs 单表统一 | **双表可见（eventfd 范式）**——最小风险；单表统一（全栈迁 substrate）另立后续 |
| 3 | ops.read/write 落点 | **直接调 net step 族**，不绕 syscall 层（避免套娃与重复 await 逻辑） |
| 4 | socketpair(AF_UNIX) | **本期不动**（OpenFileBacking::SocketPair 是 pipe 对，独立形态）；特判保留，记账 |

---

*P3-A 完成后：socket 与 eventfd/pipe 在"fd 抽象、就绪、等待"三个维度完全同构；P3-B（模型瘦身/单锁/D7）与 P3-C（R1/R2 族）在此地基上另立执行文档。*
