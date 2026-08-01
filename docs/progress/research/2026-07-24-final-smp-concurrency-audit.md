# 2026-07-24 final-smp 并发完整性审查

## 结论

当前分支已经具备 AP 上线、每 hart 运行队列、远程唤醒以及“子任务首次分散后固定
在目标 hart”的基础能力，但还不能把用户态执行视为完整的 SMP 实现。主要缺口不在
“多加几把锁”，而在跨 hart 生命周期协议尚未闭合：

1. 页表根、ASID 驻留和 TLB shootdown 的所有权协议不完整。
2. 进程 clone/exit/exec 没有事务化，也没有等待远端线程停止的两阶段协议。
3. fd 分配与安装分成两个临界区，同一进程的并发 open/dup 会争用同一个 fd。
4. EBR 的 CPU pin 只是类型标记，没有实际的禁止迁移/抢占语义。

因此，当前 BuildStorm SMP 出现的随机用户态 SIGSEGV 不应继续按单个 crate 或单个
缺页点打补丁。首先必须修复下列 P0 协议。

## 本轮实施状态

截至 2026-07-24，本审查列出的 1–8 项已经在 `final-smp` 工作区实现：

1. RV64/LA64 均按 hart 记录活动用户页表和 ASID；根销毁等待驻留掩码清零。
2. 两个架构的 TLB shootdown 均使用带类别的 IPI pending/ack，远端完成失效后再确认。
3. RV64/LA64 离开用户态时先切回永久内核根，再清除 ASID 驻留。
4. 进程增加 `Alive/Execing/Exiting/Zombie` 生命周期，clone 先 attach、最后发布 TID；
   exec guard 覆盖 sibling collapse 至地址空间提交完成；exit/exec 等待远端线程退出活动
   hart 集合后再释放共享资源。
5. fd 的“找空位、安装文件、设置 CLOEXEC”改为同一事务，open/dup/pipe/socket 等生产
   路径统一使用该接口，失败路径补齐 pipe/socket 引用回滚。
6. 异步 VM writer 持有同一 `PendingWriter` ticket 跨 wait，避免缺页 materializer
   无限插队。
7. CPU pin 改为每 hart 可嵌套 pin depth；Guard 为 `!Send/!Sync`，用户态入口强制检查
   depth 为零。当前内核 poll 不可抢占，因此无需为每次 EBR/zone Guard 屏蔽中断。
8. robust-list 只在 process 锁内取得地址空间能力，用户链表读取和 futex wake 均在锁外。

第 9 项仍是明确的架构性性能债：`Ext4Pager` 的可变镜像、inode/extent 缓存和同步
`FsOps` 接口目前共同依赖整挂载点锁。安全拆分需要先把块 I/O 改成可等待事务，再划分
超级块/位图、inode 和目录锁；不能仅把一个锁机械替换成多把锁，否则会引入锁序反转
或破坏 ext4 更新原子性。

## P0：可直接造成错误结果或内存破坏

### 1. LA64 活动页表记录是全局变量，并非 per-hart

证据：

- `boards/tx-hal-loongarch64-qemu-virt/src/lib.rs:100-102` 用三个全局
  `AtomicUsize` 保存活动 PGDL/PGDH/ASID。
- `la64_pmap.rs:178-201` 根据这些全局值判断当前 hart 是否需要切换页表。

hart A 写入“页表 X 已活动”后，hart B 可能把全局值改成 Y。更危险的是，hart B
也可能因为全局值碰巧等于目标值而跳过自身 CSR 的实际切换。这会让一个 hart 在错误
的进程页表上运行。

同时：

- `platform_impls.rs:247-250` 直接递归释放页表根和 ASID，没有远端驻留等待。
- `platform_impls.rs:303-304` 的用户页表 shootdown 只在当前 hart 执行
  `invtlb`，没有远端 hart 的 IPI/ack。

这意味着一个线程 `munmap/mprotect` 后可以释放物理页，而另一个 hart 仍持有指向
该物理页的旧 TLB 项。

修复要求：

- 活动 PGDL/PGDH/ASID 改为 per-hart 状态，或者直接读取当前 hart CSR。
- 建立每个根/ASID 的 hart 驻留掩码。
- LA64 shootdown 必须向驻留 hart 发 IPI，远端 `invtlb` 后回 ack；调用方收到全部
  ack 后才允许释放 `MapPin`、页表页和 ASID。
- 销毁根页表前必须等待所有 hart 切换到永久内核根。

### 2. RV64 在硬件仍使用用户 SATP 时提前清除 ASID 驻留

`trap.rs:1107-1115` 在用户态 trap 返回 reactor 时先调用
`clear_current_asid_residency()`，但没有先把 SATP 切换到永久内核根。于是软件认为
该 hart 已离开此 ASID，硬件实际上仍使用该根。

后果：

- 另一个 hart 的 shootdown 会漏掉这个 hart。
- 此 hart 下次进入同一 ASID 时可能继续使用旧 TLB。
- 进程根/ASID 在其他 hart 被回收复用时，本 hart 的 SATP 仍可能引用旧根。

代码自身已在 `pmap/address_space.rs:505-513` 注明销毁协议当前只保证本 hart，
多核保证尚未实现。

修复顺序必须是：

1. 当前 hart 切到永久内核根。
2. 本地全量 fence。
3. 再清除该 ASID 的驻留位。
4. 根销毁方等待驻留掩码清零；必要时主动 IPI 请求远端切根并等待 ack。

### 3. clone 发布不是事务，能产生“有 TID、无进程归属”的线程

`process/execution.rs:760-832` 当前顺序是：

1. 创建线程。
2. 先 `register_tid`。
3. 最后才尝试在 `process.payload` 中 attach。
4. 如果进程已经退出、payload 为 `None`，函数仍返回 `Ok(child)`。

并发 `clone` 与 `exit_group` 时会产生已注册、甚至可提交给 reactor，但不在进程
线程表中的孤儿线程。

正确事务：

- 在进程生命周期锁下确认 `Alive` 且未进入 group-exit，取得 clone reservation。
- 构造子线程但不对外发布。
- attach 线程表并更新计数。
- 最后发布 TID 和 reactor task。
- 任一步失败均逆序回滚。

### 4. exit_group/exec 没有远端线程停止与确认协议

`step_exit_group` 在 `process.payload` 自旋锁内直接执行 shm detach、fd drain、
文件 flush、线程表 drain 和 zombify（`process/execution.rs:893-946`）。它没有等待
其他 hart 上正在运行的兄弟线程离开用户态/系统调用。

`collapse_threads_for_exec` 也只是从发起线程所在 hart 直接调用其他线程的
`step_thread_exit`，随后立即清除 `group_exit`（`process/structure.rs:1607-1633`）。
现有 `remaining_threads` 结构并没有形成生产路径上的等待屏障。

结果是远端线程仍可能：

- 使用正在关闭的 fd/socket；
- 使用已从进程 payload 撤出的地址空间和信号状态；
- 与 exec 的新地址空间提交并行运行；
- 在另一个 hart 正在释放资源时继续系统调用。

正确协议应是两阶段：

1. 生命周期锁下 `Alive -> Exiting`，禁止新 clone，发布停止代次。
2. IPI/唤醒所有兄弟任务；每个线程离开用户态和内核临界区后 ack。
3. 发起方等待全部 ack。
4. 锁外执行 robust futex、fd close/fsync、shm detach 等慢操作。
5. 最后销毁地址空间并发布 Zombie/SIGCHLD。

### 5. fd “查找空位”和“安装”不是一个原子操作

`ProcessPayload::allocate_fd_at_least` 只在 fd 锁内扫描后返回号码
（`process/structure.rs:1648-1662`），`openat` 随后在另一次锁操作中
`set_fd`（`fs_basic.rs:1140-1149`）。

同一进程的两个线程可以同时得到同一个空 fd，后安装者覆盖前一个 `OpenFile`。
`fd_cloexec` 又位于另一把锁中，使 fd 与 CLOEXEC 位也无法原子发布。

修复要求：

- 增加 fd reservation/transaction，或在同一 fd-table 锁内完成
  “找空位 + 限额检查 + 安装 + CLOEXEC”。
- open、dup、pipe、socket、eventfd 等所有产生 fd 的路径统一使用该接口。
- close/exec-cloexec 与 reservation 定义清晰的冲突顺序。

## P1：会造成死锁、饥饿或严重串行化

### 6. RangeLock 的 writer preference 在生产接口中被立即丢弃

`range_lock.rs:138-162` 明确说明生产 `acquire_step` 会把 rich
`WouldBlock` 立即丢弃。随之析构的 `PendingWriter` 也撤销了等待 writer 的登记。
持续并发缺页的 Materializer 可以无限插队，使 `munmap/mprotect/mremap` writer
长期饥饿。

需要让异步 VM script 持有 `PendingWriter` 跨 wait，醒来后用同一个 writer ticket
重试；不能只保存 wait-source id。

### 7. EBR 的 CPU pin 没有真实 pin 行为

`tx-hal/src/lib.rs:69-85,1064-1066` 的 `CpuPinGuard` 只保存 CPU id 和
`!Send/!Sync` 标记，Drop 不恢复任何状态，平台也没有覆盖 `pin_current_cpu()`。

在当前“任务只在 poll 边界切换、Guard 不跨 await”的约束下，它通常不会立刻出错；
但它并不满足 EBR 注释声称的“Guard 存活期间禁止迁移”。一旦启用 poll 内核抢占或
更自由的任务迁移，同一个 Guard 会在错误的 per-CPU epoch slot 上 unpin。

需要二选一：

- 明确保证内核 poll 不可抢占，并把该保证写成 HAL/调度器可检查的不变量；或
- 实现真实 preempt-disable/pin depth，Guard Drop 时恢复。

### 8. robust-list 在进程 payload 锁内读取最多 2048 个用户节点

`thread_runtime/execution.rs:410-468` 持有 `process.payload` 自旋锁进行用户地址读取、
缺页处理和 futex wake。这个临界区既长，又会进入 VM/futex 子系统，容易形成锁序
反转。

应在锁内只复制 `AddressSpace Cap + robust head/len`，立即释放 process 锁，再走
用户链表。

### 9. ext4 仍是整挂载点的同步自旋锁

`tx-ext4/src/read_backend.rs:670-715` 用一个 `Ext4PagerCell` 锁住整个 pager；
lookup、inode 更新、extent 分配、块 I/O 都串行。它目前主要是性能问题而非已确认
的数据损坏问题，但多核下一个 hart 做慢块 I/O时，其他 hart 会一直占 CPU 自旋。

后续应拆分超级块/分配位图锁、inode 锁和目录锁，并让块 I/O 走可等待的异步路径，
而不是持自旋锁等待设备。

## 已完成或不应重复归因的部分

- 当前分支的 reactor 已修复 `Polling` 期间 wake 被提前消费导致永久 Parked 的竞态。
- PageBacked 已有同页 in-flight fetch 合并，分配和 MapPin 已移出 PC 元数据锁。
- 本轮已给 VM fault publish 增加“recipe 发生并发变化就重新从头解析”的 retry，
  这修复的是 stale-recipe 症状，但不能替代页表/TLB 和进程生命周期协议。
- 子用户任务目前是“首次 spread，然后 pinned”，所以尚未开启 trap 后跨 hart
  迁移；这降低了风险，但不能阻止多个不同线程/进程同时在多个 hart 上修改共享状态。

## 建议实施顺序

1. 先修 RV64/LA64 页表驻留、远端 shootdown、切根和销毁 ack。
2. 再实现 Process `Alive/Exiting/Zombie` 状态机及 clone/exit/exec 两阶段事务。
3. 原子化 fd reservation/install。
4. 修 RangeLock writer ticket 和 robust-list 锁边界。
5. 最后处理 ext4/tmpfs 的锁拆分与性能。

在前两项完成前，不建议开启 post-trap 用户线程迁移或 work stealing；保留“首次
spread 后 pinned”更容易定位剩余共享状态问题。
