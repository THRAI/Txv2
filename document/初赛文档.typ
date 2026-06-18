// =============================================================================
// Txv2 操作系统内核设计文档 —— 初赛文档
// 版式骨架 + 第10章(Substrate)对象存储/延迟回收 正文。
// 校徽:同目录 校徽.jpg。编译:typst compile 初赛文档.typ
// =============================================================================

#let project-name = "Txv2"
#let doc-subtitle = "设计文档"
#let team         = "（参赛队名待填）"
#let members      = "（队伍成员待填）"
#let advisors     = "夏文、仇洁婷"
#let date-text    = "2026 年 6 月"
#let header-text  = project-name + " " + doc-subtitle

// ----------------------------- 全局版式 --------------------------------------
#set page(paper: "a4", margin: (top: 2.5cm, bottom: 2.5cm, left: 3cm, right: 3cm))

// 正文:西文统一 Noto Serif,中文宋体;小四;每段首行缩进 2 字(含标题后首段)
#set text(font: ("Noto Serif", "SimSun", "Noto Serif CJK HK"), lang: "zh", region: "cn", size: 12pt)
#set par(justify: true, leading: 1em, first-line-indent: (amount: 2em, all: true))

// 代码:等宽字体;代码块浅灰底、圆角、内边距
#show raw: set text(font: ("DejaVu Sans Mono",), size: 9.5pt)
#show raw.where(block: true): it => block(
  fill: luma(246), inset: (x: 10pt, y: 8pt), radius: 4pt,
  width: 100%, breakable: true, it,
)

// 标题编号:一级 = “第N章”,二级 = “N.M”,三级 = “N.M.K”
#set heading(numbering: (..n) => {
  let nums = n.pos()
  if nums.len() == 1 { "第" + str(nums.at(0)) + "章" } else { nums.map(str).join(".") }
})
// 标题:西文同样用 Noto Serif(全局统一),中文用黑体;不加粗,靠字体与字号区分层级
#show heading: set text(font: ("Noto Serif", "Noto Sans CJK SC", "Microsoft YaHei"), weight: "regular")
#show heading.where(level: 1): it => block(above: 1.5em, below: 1.0em, text(size: 18pt, it))
#show heading.where(level: 2): it => block(above: 1.1em, below: 0.6em, text(size: 15pt, it))
#show heading.where(level: 3): it => block(above: 0.9em, below: 0.5em, text(size: 13pt, it))

// =============================================================================
// 封面
// =============================================================================
#page(header: none, footer: none)[
  #set align(center)
  #v(0.6cm)
  #image("校徽.jpg", width: 100%)
  #v(1.0cm)
  #text(font: ("Noto Serif",), size: 40pt, weight: "bold")[#project-name]
  #v(0.6cm)
  #text(size: 22pt)[#doc-subtitle]
  #v(3.0cm)

  #let info-line(value) = box(
    width: 7.5cm, inset: (bottom: 5pt), stroke: (bottom: 0.6pt + black),
  )[#align(center)[#value]]

  #set text(size: 16pt)
  #grid(
    columns: (auto, auto), column-gutter: 1em, row-gutter: 1.8em,
    align: (right + bottom, left + bottom),
    [参赛队名], info-line(team),
    [队伍成员], info-line(members),
    [指导老师], info-line(advisors),
  )
  #v(2.4cm)
  #text(size: 15pt)[#date-text]
]

// =============================================================================
// 前置:摘要 + 目录(罗马页码;目录只到二级)
// =============================================================================
#set page(
  numbering: "I",
  header: { set text(size: 9pt); align(center)[#header-text]; line(length: 100%, stroke: 0.5pt) },
)
#counter(page).update(1)

#heading(level: 1, numbering: none, outlined: true)[摘要]

// (摘要正文待填)

#heading(level: 2, numbering: none, outlined: false)[模块完成情况]

#table(
  columns: (10em, 1fr), inset: 8pt, align: (left + horizon, left + horizon),
  table.header([*模块*], [*完成情况*]),
  [对象模型], [（待填）], [执行模型], [（待填）],
  [进程与信号], [（待填）], [内存管理], [（待填）],
  [文件系统], [（待填）], [网络], [（待填）],
  [进程间通信], [（待填）], [设备], [（待填）],
  [基础设施], [（待填）], [硬件抽象层], [（待填）],
  [系统调用兼容], [（待填）], [可观测性], [（待填）],
)

#pagebreak()
#outline(title: [目　录], depth: 2, indent: 1.5em)

// =============================================================================
// 正文(阿拉伯页码,从 1 起)
// =============================================================================
#pagebreak()
#set page(numbering: "1")
#counter(page).update(1)

= 概述
== 项目介绍
== 整体架构
== 设计特色
== 分工与贡献
== 参考与改进

= 对象模型
== 身份与 Payload
== 引用与生命周期
== 绑定与义务
== 子系统结构
== 命名空间

= 执行模型
== Step 模型
== 异步与让出
== 脚本与上下文
== Reactor 调度
== 委托与作用域
== 线程运行时

= 进程与信号
== 进程与线程
== 进程创建与执行
== 凭证与资源限制
== 信号机制

= 内存管理
== 物理内存管理
== 地址空间
== 页缓存
== 缺页异常
== VDSO

= 文件系统
== 虚拟文件系统
== 挂载
== 磁盘文件系统
== 伪文件系统
== 块设备与页缓存

= 网络
== 套接字
== 传输层
== 网络层与设备
== netfilter 与路由
== 网络命名空间

= 进程间通信
== System V IPC
== POSIX 消息队列
== 管道与 futex
== 事件通知
== 异步 I/O

= 设备
== 设备模型
== 设备驱动
== 终端

= Substrate 基础设施层

Txv2 将对象生命周期、并发回收、发布与事务等横切关注点,从各子系统中抽出,统一沉淀为一层基础设施 (substrate)。该层借鉴 Asterinas OSTD 的 framekernel 思想:上层子系统不再各自发明这些机制,而是统一构建在 substrate 之上,从而减少重复、收窄出错面。本章介绍其中最具特色的两部分——对象存储与延迟回收;物理帧分配器与内核堆属于内存管理,见第 5 章。

== 对象存储

内核中绝大多数对象都需要可回收的生命周期。许多类 rCore 内核直接用 `Arc<T>` 管理对象,但这会让 pid 表、路径遍历等查找热路径背负引用计数的原子开销,也无法表达 Linux 中"身份还在、实体已死"的分层语义(如僵尸进程、已 `unlink` 但仍打开的文件)。为此,Txv2 不用 `Arc`,而是为每一种可回收类型提供一个专属的、由物理页帧支撑的对象池 (Zone),并把"短期遍历安全"与"长期语义保留"分开承载。其存储层次自上而下为:

```text
Zone<T>            每种类型一个对象池
 ├─ ZoneBucket     每 CPU 空槽缓存(免锁)
 └─ Keg            中心 slab 管理器
     └─ ZoneSlab   一页物理帧
         └─ Slot   单个对象槽(状态字 + 对象)
```

=== 数据结构

最顶层是 `Zone<T>`。每种可回收类型 `T` 对应一个静态实例,概念上等同于为每种对象建立一个 `kmem_cache`,并通过 `ZoneAllocated` trait 将类型绑定到其唯一的 zone。`Zone<T>` 持有该类型的全部存储与每 CPU 缓存:

```rust
pub struct Zone<T: 'static> {
    id: AtomicUsize,                 // 在全局注册表中的编号
    allocated_slots: AtomicUsize,    // 当前拥有的槽位总数
    keg: Keg<T>,                     // 中心 slab 管理器
    buckets: [UnsafeCell<ZoneBucket<T>>; MAX_ZONE_CPUS],  // 每 CPU 空槽缓存
}
```

其中 `buckets` 是每个 CPU 各一份的空槽缓存:分配对象时优先从本 CPU 的缓存取槽,靠 CPU 绑定避免加锁;缓存用尽才向中心的 `keg` 批量补给。`keg` 是该类型所有 slab 的中心管理者,用三条链表按占用情况组织 slab,并以一张 `slab_id` 到 slab 的索引,加速由对象句柄反查其所在槽位:

```rust
pub struct Keg<T: 'static> {
    lock: SpinLock,
    partial_head: *mut ZoneSlab<T>,  // 部分占用(分配时优先)
    full_head:    *mut ZoneSlab<T>,  // 全满
    empty_head:   *mut ZoneSlab<T>,  // 全空(保留一个备用,多余者经回收归还)
    slab_index: BTreeMap<usize, NonNull<ZoneSlab<T>>>,
    // slab_count / empty_count / next_slab_id ...
}
```

每个 `ZoneSlab<T>` 是一页物理帧,页头之后紧跟一段密集排布的槽数组,用一个 64 位位图标记空闲槽,因此一页最多容纳 64 个槽。所属 zone 只在 slab 头记录一次,槽本身不再各自保存回指针:

```rust
pub struct ZoneSlab<T: 'static> {
    id: usize,                 // 该 slab 的编号
    zone: &'static Zone<T>,    // 所属对象池
    backing_ppn: Ppn,          // 支撑的物理帧号
    slot_count: usize,         // 本页的槽数
    free_count: usize,         // 空闲槽数
    free_bitmap: u64,          // 每位对应一个槽,置位表示空闲
    list: SlabList,            // 当前所在链表(partial/full/empty)
    next: *mut ZoneSlab<T>,    // 侵入式链表指针
}
```

最底层是 `Slot<T>`,即单个对象槽,由一个打包的状态字与对象存储组成;`MaybeUninit` 表示空槽时其中并无合法对象:

```rust
pub struct Slot<T: 'static> {
    meta: SlotMeta,                    // 打包的生命周期状态字
    value: UnsafeCell<MaybeUninit<T>>, // 对象存储
}
```

状态字 `SlotMeta` 是一个原子 `u64`,把"状态、强引用计数、代号"三者打包在一起,使升级、克隆、释放都能用一次比较交换 (CAS) 完成全状态校验。状态机为五态,代号在每轮回收后加一:

```rust
pub enum SlotState { Free, Reserved, Live, Dead, Retiring }
// 位布局:[2:0]=state, [47:16]=retain(强引用计数), [63:48]=generation(代号)
```

`Free → Reserved → Live` 是分配与发布,`Live → Dead → Retiring → Free` 是死亡与回收。代号用于防止 ABA:槽位被复用后,旧的弱引用会因代号不符而失效。

=== 引用与使用

对象不对外暴露裸指针,只暴露三种强弱分明的句柄:

```rust
pub struct Cap<T>          { raw: u32 }                  // 强持有,4 字节
pub struct Weak<T>         { raw: u32, generation: u16 } // 弱提示,8 字节
pub struct IdentRef<'g, T> { /* guard 作用域内的临时观察 */ }
```

`Cap<T>` 是身份保活:只要它存在,槽位就不会被回收,可跨步骤、跨线程长期持有;因其保活,代号不会变,故无需存代号,仅占 4 字节。`Weak<T>` 只记录槽位地址与代号快照,不保活、不阻止对象死亡,使用前须在保护期内校验代号。`IdentRef<'g, T>` 是 EBR 保护期内的临时观察,其生命周期受编译器约束,无法逃出读侧临界区。三者构成单向升级链:`Weak` 在保护期内观察得到 `IdentRef`,再升级为 `Cap`;这一升级是一次同时校验代号、状态并增加引用计数的 CAS,是无锁观察与引用计数之间唯一的线性化点。

对象的创建采用两阶段协议:先 `reserve` 取得一个处于 `Reserved` 的槽并返回线性凭证,确认无误后再 `sign` 写入对象并发布为 `Live`;凭证若未发布即被丢弃,会自动回滚为 `Free`。这与内核"先预留资源、全部成功再统一提交"的步骤纪律一致。

对于身份生命周期长于实体的对象,Txv2 进一步将其拆为身份与实体两个独立的 zone,如 `ProcessIdentity` 与 `ProcessPayload`:进程退出时释放实体槽(地址空间、fd 表等),但身份槽连同 pid、退出码、父子关系保留至父进程回收,从而自然表达僵尸进程语义。套接字、挂载点、System V IPC 等同样采用这一拆分。

== 延迟回收

Zone 中由弱提示观察对象的过程是无锁的:它只在保护期内读取槽位元数据而不增加引用计数,因此 pid 表、路径遍历等查找热路径几乎没有原子开销。但这带来一个根本问题——当一个观察者正持有临时引用读取某对象时,另一个 CPU 可能恰好释放该对象、并把它所在的物理页归还帧分配器,于是观察者读到的便是已被复用的内存。引用计数能避免这一点,代价却正是热路径上的原子读改写;为兼得"读侧零成本"与"释放安全",Txv2 采用基于纪元的回收 (EBR, Epoch-Based Reclamation)。其思路与 Asterinas OSTD 的 RCU 一致:读者进入临界区时几乎不付代价,被删除对象的物理释放则推迟到"所有可能看到它的读者都已离开"之后,这段等待称为宽限期 (grace period)。

EBR 的核心是一个粗粒度的逻辑时钟——纪元 (epoch)。系统维护一个全局纪元,读者进入临界区时把当前全局纪元"登记"到自己所在的 CPU;回收方据此判断:一个在纪元 E 退休的对象,必须等到全局纪元推进到足以保证"不再有任何读者停留在 E 或更早的纪元"时,才可真正释放。

=== 数据结构

EBR 的全局状态集中在 `EpochDomain`,其中并发热点字段都做了缓存行隔离,避免 CPU 间的伪共享:

```rust
struct EpochDomain {
    global_epoch:  CachePadded<AtomicU64>,    // 全局纪元
    active_guards: CachePadded<AtomicUsize>,  // 当前活动的临界区计数
    possible_cpus: CachePadded<AtomicUsize>,  // 参与回收的 CPU 数
    cpu_states: [CpuLocalEpochState; MAX_EPOCH_CPUS],  // 每 CPU 状态
    // 每 CPU 退休对象池、平台钩子 ...
}
```

每个 CPU 拥有一份 `CpuLocalEpochState`,记录该 CPU 当前登记的纪元与它自己的待回收链表;`local_epoch` 为 0 表示该 CPU 不在任何临界区中:

```rust
struct CpuLocalEpochState {
    initialized: AtomicBool,          // 是否已加入纪元域
    local_epoch: AtomicU64,           // 0 表示静默;非零表示正处于某纪元的临界区
    retired: UnsafeCell<RetiredList>, // 本 CPU 的待回收链表(仅本 CPU 在 pin 时改)
}
```

对象死亡后会被登记为一个退休结点。结点为定长结构,链表以数组下标而非指针相连,使退休路径上完全不触发动态分配;每个 CPU 各持一个定长退休池:

```rust
struct RetiredNode {
    ptr: *mut u8,                   // 待回收对象
    reclaim_fn: unsafe fn(*mut u8), // 对象专属的析构/回收回调
    retired_at_epoch: u64,          // 退休时观察到的全局纪元
    next: Option<usize>,            // 下一结点在池中的下标
}

struct PerCpuRetiredPool {
    nodes: [RetiredNode; RETIRED_NODE_POOL_CAPACITY],  // = 1024
    free_head: Option<usize>,
}
```

=== 回收机制

读者访问 zone 对象前调用 `epoch::guard()` 进入临界区,其流程为:确认运行期已初始化、且当前不在中断上下文;绑定当前 CPU 防止迁移;读取全局纪元并断言本 CPU 当前为静默(临界区不可嵌套);随后把全局纪元写入本 CPU 的 `local_epoch`、递增活动计数,并插入一个 `SeqCst` 屏障。这一屏障是读侧的核心顺序保证:它确保"已登记纪元"对其它 CPU 可见之后,受保护的读取才会真正发生,从而不被重排到登记之前。临界区结束时把 `local_epoch` 置回 0。由于不可嵌套,对于"已在临界区内、下层接口又要求传入 `&Guard`"的场景,另提供 `borrow_current_guard()`:借用当前已登记的纪元,不重复进入、不改计数、析构为空操作。

全局纪元的推进受严格约束:只有当每一个在线且已初始化的 CPU 都处于静默、或都已登记到当前纪元时,才允许把全局纪元加一(以一次 CAS 完成);只要还有任何 CPU 停留在更早的纪元里,纪元就不会前进:

```rust
// 纪元推进的判定(简化):没有任何 CPU 落后于当前纪元才可前进
for cpu in 在线且已初始化的 CPU {
    let local = cpu.local_epoch;
    if local != 0 && local < current { return false; }  // 仍有读者停留在更早纪元
}
global_epoch.compare_exchange(current, current + 1, ...)
```

对象死亡时并不立即析构,而是被"退休":记录下当前全局纪元并登记进本 CPU 的退休链表。回收发生在带预算的 `try_drain(budget)` 中——它先尝试推进全局纪元,取当前纪元为 `safe_epoch`,然后只扫描本 CPU 的退休链表(其它 CPU 各自回收自己的,彼此不加锁),对每个结点判断

```text
safe_epoch >= retired_at_epoch + 2
```

成立且未超预算者执行其回收回调,否则放回链表等待下一轮。回收回调由对象自身提供,纪元域只负责决定"何时安全",二者职责分离;回调执行完毕后结点归还空闲链表以复用。预算上限避免了在热路径或维护周期里做无界析构。

这里的关键是"加二"的安全窗口。一个对象在纪元 E 退休时,可能正有读者也处于纪元 E(它在退休前就已观察到该对象)。要把全局纪元从 E 推进到 E+1,必须确保没有任何 CPU 还停留在比 E 更早的纪元;再从 E+1 推进到 E+2,则必须确保没有 CPU 还停留在 E。因此当全局纪元到达 E+2 时,所有纪元 E 的读者都已离开,该对象方可安全释放——两个纪元的间隔,即是这套机制的宽限期。

=== 与对象存储的协同

EBR 与 Zone 协同完成"对象之死"。当最后一个 `Cap` 释放时,槽位先经一次 CAS 进入 `Dead` 并装上禁止升级的哨兵——此时任何新的升级都会失败,但对象内存尚未析构,因为可能仍有旧的临时引用在某个临界区内安全读取;随后槽位转入 `Retiring`,并通过 `epoch::retire` 登记退休。只有当 EBR 经由上述的纪元推进与安全窗口,证明没有任何旧临界区还能触达该槽位之后,回收回调才真正运行:析构对象、把代号加一、将状态置回 `Free`、并把槽位归还所属 keg 以备复用。由此,"短期遍历安全"由 EBR 的宽限期保证,"长期语义保活"由 `Cap` 的引用计数保证,二者在同一个槽位的原子状态字上协同,共同支撑起 Txv2"读侧无锁、释放安全、并能表达分层生命周期"的对象模型。

== 发布与等待

== 事务化变更

= 硬件抽象层
== 抽象层设计
== 启动与多核
== 页表管理
== 陷入与中断
== 双架构支持

= 系统调用兼容
== 分发机制
== 系统调用覆盖
== ABI 兼容

= 可观测性
== 分层追踪
== 追踪重建

= 测试与性能
== 测试环境
== 功能测试
== 性能测试

= 总结与展望
== 工作总结
== 经验总结
== 项目意义
== 未来计划
