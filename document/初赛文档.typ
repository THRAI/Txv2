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

进程、套接字、挂载点这些内核对象,语义各不相同,但都得面对同样几个底层问题:在哪里分配、什么时候回收、回收的时候怎么保证别的 CPU 不会正好在读它、以及多个修改之间怎么不打架。这些事情如果让每个子系统各写一套,既重复又容易出错。我们的做法是把它们从子系统里抽出来,单独做成一层基础设施,叫 substrate,设计上参考了 Asterinas OSTD 把底层机制收归框架的思路:上层不再自己造这些轮子,统一建在 substrate 之上。本章只讲其中和别的内核区别最大的两块——对象存储和延迟回收;物理帧分配器和内核堆也在这一层,但更贴近内存,放到第 5 章讲。

== 对象存储

很多 rCore 系的内核直接用 `Arc<T>` 来管内核对象,简单,但有两个绕不开的问题。一是热路径上的开销:pid 表查找、路径遍历这些操作,每次拿到对象都要对引用计数做一次原子加减,而它们偏偏是调用最频繁的地方。二是表达力不够:`Arc` 只有"活着"和"没了"两种状态,描述不了 Linux 里很常见的"身份还在、实体已经死了",比如僵尸进程、已经 `unlink` 但还被打开的文件。

我们没有用 `Arc`,而是给每一种需要回收的类型单独开一个对象池,叫 Zone。一个 `Zone<T>` 由物理页帧支撑,同类对象密集地排在这些页里,分配和回收一个对象,本质上就是在页里占用或让出一个槽——这其实就是 Linux slab 的思路,为每种对象建一个 cache。不一样的地方在于,我们给每个槽加了一套状态机,把"短期能不能安全读"和"长期还活不活着"分开记;后面会看到,这正是同时做到无锁查找和分层语义的关键。

Zone 的存储自上而下分四层:

```text
Zone<T>            每种类型一个对象池
 ├─ ZoneBucket     每 CPU 空槽缓存(免锁)
 └─ Keg            中心 slab 管理器
     └─ ZoneSlab   一页物理帧
         └─ Slot   单个对象槽(状态字 + 对象)
```

=== 数据结构

最上面是 `Zone<T>`,每种类型对应一个静态实例,通过 `ZoneAllocated` trait 把类型绑定到它唯一的 zone。它持有这种类型的全部存储,还有每个 CPU 一份的空槽缓存:

```rust
pub struct Zone<T: 'static> {
    id: AtomicUsize,                 // 在全局注册表中的编号
    allocated_slots: AtomicUsize,    // 当前拥有的槽位总数
    keg: Keg<T>,                     // 中心 slab 管理器
    buckets: [UnsafeCell<ZoneBucket<T>>; MAX_ZONE_CPUS],  // 每 CPU 空槽缓存
}
```

`buckets` 是每个 CPU 各一份的空槽缓存。分配对象时先从本 CPU 的缓存里拿槽,靠 CPU 绑定就不用加锁,只有缓存用光了才去找中心的 `keg` 批量补一批回来——为的是让最常走的分配路径不碰锁。`keg` 是这种类型所有 slab 的中心管理者,它用三条链表按占用情况把 slab 分开放;另外,从对象句柄反查它落在哪个槽(`slot_from_key`)在每次 `Cap` 解引用/克隆/释放时都要做,原本顺着链表找是 O(slab 数),于是加了一张 64 项的直接映射缓存(按 `slab_id & 63` 索引、用 slab 自身单调 id 校验),把它降到 O(1):

```rust
const SLAB_CACHE_SIZE: usize = 64;

pub struct Keg<T: 'static> {
    lock: SpinLock,
    partial_head: *mut ZoneSlab<T>,  // 部分占用(分配时优先)
    full_head:    *mut ZoneSlab<T>,  // 全满
    empty_head:   *mut ZoneSlab<T>,  // 全空(保留一个备用,多余者经回收归还)
    slab_cache: [*mut ZoneSlab<T>; SLAB_CACHE_SIZE],  // slab_id&63 → slab,反查 O(1)
    // slab_count / empty_count / next_slab_id ...
}
```

每个 `ZoneSlab<T>` 就是一页物理帧。页头放元数据,后面紧跟一排槽,哪些空闲用一个 64 位位图记,所以一页最多放 64 个槽。所属的 zone 只在页头记一次,槽自己不再存回指针,省下的空间都留给对象:

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

最底层是 `Slot<T>`,一个槽,就是状态字加对象存储两部分。空槽的时候里头没有合法对象,所以用 `MaybeUninit` 包着:

```rust
pub struct Slot<T: 'static> {
    meta: SlotMeta,                    // 打包的生命周期状态字
    value: UnsafeCell<MaybeUninit<T>>, // 对象存储
}
```

状态字 `SlotMeta` 是一个原子 `u64`,把状态、强引用计数、generation三样东西打包进同一个字。打包的好处是,升级、克隆、释放这些操作都能用一次 CAS 把所有条件一起校验、一起改,不必分几步加锁。状态机一共五态,generation每回收一轮加一:

```rust
pub enum SlotState { Free, Reserved, Live, Dead, Retiring }
// 位布局:[2:0]=state, [47:16]=retain(强引用计数), [63:48]=generation
```

`Free → Reserved → Live` 走的是分配和发布,`Live → Dead → Retiring → Free` 走的是死亡和回收。generation是用来防 ABA 的:一个槽回收后再被分出去,generation已经变了,之前残留的弱引用拿generation一对就知道对不上,自动失效。

=== 引用与使用

对象从不对外给裸指针,只给三种句柄,强弱分得很清楚:

```rust
pub struct Cap<T>          { raw: u32 }                  // 强持有,4 字节
pub struct Weak<T>         { raw: u32, generation: u16 } // 弱提示,8 字节
pub struct IdentRef<'g, T> { /* guard 作用域内的临时观察 */ }
```

`Cap<T>` 是强持有,只要它还在,槽就不会被回收,可以跨步骤、跨线程长期拿着;正因为它保活,generation不可能变,也就不用存generation,所以只要 4 字节。`Weak<T>` 是弱提示,只记槽的地址和当时的generation,不保活,也拦不住对象死亡,用之前必须在保护期内拿generation校验一遍。`IdentRef<'g, T>` 是临界区里的临时观察,它的生命周期被编译器绑在保护期上,跑不出读侧临界区。三者是单向往上升的:`Weak` 在保护期内先看到 `IdentRef`,再升成 `Cap`。这一步升级是一次 CAS,同时把generation、状态校验掉、再加上引用计数——无锁观察和引用计数就在这一点上交汇。

建一个对象分两步:先 `reserve` 占一个槽,它进入 `Reserved`,同时拿到一张凭证;检查都过了再 `sign`,把对象写进去、发布成 `Live`。要是凭证还没发布就被丢掉(比如中途出错提前返回了),槽会自动回滚成 `Free`。这跟内核里"先把资源都预留好、确认没问题再统一提交"的做法是一致的。

还有一类对象,身份比实体活得久,我们就把它拆成两个独立的 zone,比如 `ProcessIdentity` 和 `ProcessPayload`。进程退出时,先把实体槽放掉(地址空间、fd 表这些),但身份槽连着 pid、退出码、父子关系一直留到父进程来回收——僵尸进程的语义就这么自然地落出来了。套接字、挂载点、System V IPC 也都是这么拆的。

== 延迟回收

前面说过,Zone 的弱提示查找是无锁的:它在保护期里只读槽的元数据,不动引用计数,所以 pid 表、路径查找这些热路径几乎没有原子开销。但无锁会带来一个麻烦:一个 CPU 正拿着临时引用读某个对象,另一个 CPU 可能刚好把这个对象回收掉、连它所在的物理页都还给了帧分配器,前一个 CPU 接着读到的就是被复用的内存了。引用计数能挡住这种情况,可代价又回到了热路径上的原子读改写。我们想两头都占——读侧不花钱,回收又安全,用的办法是基于 epoch 的回收(EBR,Epoch-Based Reclamation)。

EBR 的算法我们参考了 Rust 生态里最成熟的实现 crossbeam-epoch。不过内核是 `no_std`,而且回收这条路上不能做堆分配,所以我们整个重写了一遍:退休结点放在固定大小的每 CPU 池里,用数组下标串成链表,再接上本项目的 HAL 来做 CPU pin 和跨核协同。思路和 Asterinas OSTD 的 RCU 一样:读者进临界区几乎不付代价,被删对象的真正释放则一直拖到"所有可能看到它的读者都走了"之后,这段等待就是宽限期(grace period)。

整套机制就靠一个粗粒度的逻辑时钟:epoch。系统维护一个全局 epoch,读者进临界区时,把当前的全局 epoch 记到自己所在的 CPU 上。回收方就看这个:一个在 epoch E 退休的对象,要等到全局 epoch 往前走得足够远、能保证再没有读者停在 E 或更早,才能真正释放。

=== 数据结构

EBR 的全局状态都集中在 `EpochDomain` 里。几个会被多核频繁碰的字段都单独做了缓存行对齐,免得它们挤在同一条 cache line 上互相拖累(伪共享):

```rust
struct EpochDomain {
    global_epoch:  CachePadded<AtomicU64>,    // 全局epoch
    active_guards: CachePadded<AtomicUsize>,  // 当前活动的临界区计数
    possible_cpus: CachePadded<AtomicUsize>,  // 参与回收的 CPU 数
    cpu_states: [CpuLocalEpochState; MAX_EPOCH_CPUS],  // 每 CPU 状态
    // 每 CPU 退休对象池、平台钩子 ...
}
```

每个 CPU 有一份 `CpuLocalEpochState`,记着这个 CPU 当前登记的 epoch,和它自己的待回收链表。`local_epoch` 是 0 就表示这个 CPU 现在不在任何临界区里:

```rust
struct CpuLocalEpochState {
    initialized: AtomicBool,          // 是否已加入epoch域
    local_epoch: AtomicU64,           // 0 表示静默;非零表示正处于某epoch的临界区
    retired: UnsafeCell<RetiredList>, // 本 CPU 的待回收链表(仅本 CPU 在 pin 时改)
}
```

对象死了之后,会被登记成一个退休结点。结点是定长的,链表也不用指针、改用数组下标相连,就是为了让退休这条路上一次动态分配都不做。每个 CPU 各有一个定长的退休池:

```rust
struct RetiredNode {
    ptr: *mut u8,                   // 待回收对象
    reclaim_fn: unsafe fn(*mut u8), // 对象专属的析构/回收回调
    retired_at_epoch: u64,          // 退休时观察到的全局epoch
    next: Option<usize>,            // 下一结点在池中的下标
}

struct PerCpuRetiredPool {
    nodes: [RetiredNode; RETIRED_NODE_POOL_CAPACITY],  // = 1024
    free_head: Option<usize>,
}
```

=== 回收机制

读者在访问 zone 对象之前,先调 `epoch::guard()` 进临界区。这一步会做几件事:确认运行期已经初始化、当前不在中断里;绑定当前 CPU 不让它迁移;读出全局 epoch,顺手断言本 CPU 现在是静默的(临界区不允许嵌套);然后把这个 epoch 写进本 CPU 的 `local_epoch`、活动计数加一,最后插一个 `SeqCst` 屏障。这个屏障是读侧最要紧的一处顺序保证:它确保"我已经登记了 epoch"这件事先对别的 CPU 可见,受保护的读取才真正开始,不会被重排到登记之前。临界区一结束就把 `local_epoch` 写回 0。因为不能嵌套,碰到"已经在临界区里了、底下的接口又要一个 `&Guard`"的情况,我们另给了个 `borrow_current_guard()`:它借用当前已经登记的 epoch,不重新进、不动计数,析构也什么都不做。

全局 epoch 不能随便往前推。只有当每一个在线、初始化过的 CPU 要么静默、要么已经登记到当前 epoch 时,才允许把全局 epoch 加一(用一次 CAS 完成);只要还有哪个 CPU 停在更早的 epoch,就不推:

```rust
// epoch推进的判定(简化):没有任何 CPU 落后于当前epoch才可前进
for cpu in 在线且已初始化的 CPU {
    let local = cpu.local_epoch;
    if local != 0 && local < current { return false; }  // 仍有读者停留在更早epoch
}
global_epoch.compare_exchange(current, current + 1, ...)
```

对象死的时候不马上析构,而是"退休":把当前的全局 epoch 记下来,挂到本 CPU 的退休链表上。真正回收发生在 `try_drain(budget)` 里,它带一个预算。这个函数先试着把全局 epoch 往前推一下,把推进后的当前 epoch 当作 `safe_epoch`,然后只扫本 CPU 的退休链表(别的 CPU 回收自己的,谁也不锁谁),对每个结点判断

```text
safe_epoch >= retired_at_epoch + 2
```

条件满足、又没超预算的,就执行它的回收回调,不满足的放回链表等下一轮。回调是对象自己提供的,epoch 域只管判断"什么时候安全",两边职责分开;回调跑完,结点还回空闲链表复用。带预算是为了不在热路径或维护周期里一次性做无上限的析构。

这里最关键的是,为什么是"加二"。一个对象在 epoch E 退休时,可能正好有读者也停在 epoch E(它在对象退休前就已经观察到了)。全局 epoch 要从 E 走到 E+1,前提是没有任何 CPU 还停在比 E 更早的 epoch;再从 E+1 走到 E+2,前提是没有 CPU 还停在 E。所以等全局 epoch 到了 E+2,epoch E 的读者必然都已经离开,这时候释放才安全。这两个 epoch 的间隔,就是这套机制的宽限期。

=== 与对象存储的协同

对象之死,是 EBR 和 Zone 一起完成的。最后一个 `Cap` 释放时,槽先用一次 CAS 进 `Dead`,装上一个禁止升级的哨兵——从这一刻起任何新的升级都会失败,但对象内存还动不得,因为可能还有旧的临时引用正在某个临界区里安全地读它。接着槽转入 `Retiring`,通过 `epoch::retire` 登记退休。一直要等 EBR 用前面那套 epoch 推进和安全窗口证明了"再没有旧临界区能碰到这个槽",回收回调才真正跑起来:析构对象、generation加一、状态写回 `Free`、把槽还给所属的 keg 等着被复用。这样一来,短期读得安不安全交给 EBR 的宽限期管,长期还活不活着交给 `Cap` 的引用计数管,两者就在同一个槽的那个原子状态字上配合,撑起了 Txv2 读侧无锁、回收安全、又能表达分层生命周期的对象模型。

== 发布与等待

内核里很多操作都要等:管道读要等有数据,`waitpid` 要等子进程退出,加锁要等锁释放。最直接的做法,是让每个能阻塞的对象自己存一份"谁在等我"的名单,再配个条件变量。这样写多了会冒出三个麻烦:对象攥着等待者的引用不放,谁也回收不了谁;各写各的唤醒,很容易把"被叫醒"当成"条件满足",于是拿着过期状态往下跑;还有就是十几个子系统把同一套东西各抄一遍。

我们把"睡眠—唤醒"整个抽出来,做成 substrate 的一组统一原语,放在 reactor 任务抽象之下——这样管道、futex、退出通道这些对象能发出唤醒,却不必反过来依赖 reactor。它要回答的就一句话:一个任务怎么安全地睡,事件发生时怎么精准、不丢地把它叫醒。这里有一条贯穿始终的原则:唤醒只是让等待变得高效,唤醒本身不是真相;任务被叫醒后,必须回头重读一遍条件,确认它真的满足了。

=== 数据结构

整套机制由三个角色组成,各管一件事。对象侧是 `WaitSource`,记着"谁在等我";它持一串订阅者,每个订阅者只用 `Weak` 弱引用指向等待者的mailbox——因为一个 source 可能比任何任务都活得久,用强引用就成了对象拽着任务的所有权倒挂:

```rust
pub struct WaitSource {
    id: WaitSourceId,
    subscribers: SpinMutex<Vec<Subscriber>>, // 订阅者:Weak<mailbox> + generation + 关心的事件位
    pending_mask: AtomicU64,                  // 已 fire 但没被活订阅者接走的位,留给后注册者补领
}
```

任务侧是 `TaskMailbox`,每个 reactor 任务一个,是它的事件mailbox:一个generation计数器、一条有界队列、一个用来叫醒异步任务的 `Waker`:

```rust
pub struct TaskMailbox {
    generation: AtomicU64,                    // generation计数器,每开一次新等待自增
    queue: SpinMutex<VecDeque<MailboxEvent>>, // 有界事件队列(上限 64)
    overflow: AtomicBool,                     // 队列满了置位
    waker: SpinMutex<Option<Waker>>,          // 当前 poll 上下文留下的 Waker
}
```

驱动侧是 `ActiveWait`,记着"我这一次到底在等谁的什么事件"。对象状态一变,就调 `WaitSource::notify(mask)` 发布:它遍历订阅者,死的(`Weak` 升级失败)顺手剔除,活的且关心的事件位有重叠的,往它mailbox里塞一个事件、再 `wake` 一下让 reactor 重新 poll:

```rust
subs.retain(|sub| {
    let Some(mailbox) = sub.mailbox.upgrade() else { return false }; // 死订阅者剔除
    if sub.interests.raw() & mask.raw() != 0 {
        mailbox.post(SourceFired { generation: sub.generation, source, interests });
    }
    true
});
```

=== 唤醒的正确性

无锁地睡、再被叫醒,难的不是机制,而是两个边界情况;这一节的设计就是冲它们来的。

一个是陈旧唤醒。generation放在任务侧,每开一次新等待就自增一次。一个迟到的、其实属于上一次等待的事件,带的是旧generation,任务复查时一对就知道对不上,直接丢掉;否则任务会被一个早就不相干的事件叫醒,还以为当前条件满足了——这正是"把唤醒当真相"的经典 bug。所以驱动只认generation一致的事件:

```rust
fn matches(&self, event: &MailboxEvent) -> bool {
    matches!(event, SourceFired { generation, source, interests }
        if *generation == self.generation       // generation必须一致,否则是陈旧事件
        && *source == self.source
        && interests.raw() & self.interests.raw() != 0)
}
```

另一个是丢失唤醒。朴素写法"检查条件 → 发现要睡 → 登记 → 睡过去",在"检查"和"登记"之间有一道缝;要是别人正好在这道缝里发布了事件,这次唤醒就丢了,任务睡死。我们的做法是不直接睡,而是先把登记准备好,再在锁的纪律下把"登记"和"重新检查一次条件"绑成原子的一步:

```rust
prepared.install_if(|| still_blocked())  // 仍阻塞才真正登记并挂起;已就绪则不睡,回去重试
```

这样发布方和复查方之间总有一个赢:要么发布方看到登记并投递,要么复查发现"其实已经就绪了别睡"。两条路都不会让任务停在过期的世界观上。此外还有两个小兜底:同一来源的连续 fire 会合并成一次唤醒(反正醒来要重读条件),队列满了就置溢出标志、把它当成"去重新看一眼来源"的提示——都是同一条原则:事件是提示,不是真相。

=== 使用

一个步骤(step)跑不动时,并不直接阻塞线程,而是返回一个"在某个 `WaitSource` 上等待"的让出意图,由 reactor 据此登记mailbox、挂起任务;事件到了再重新 poll、过滤陈旧、回去重读条件。正因如此,管道、eventfd、信号、System V IPC 等十几个子系统都不必各自实现等待,统一经 `wake::notify` 这一个入口发布唤醒,观测埋点也集中在这一处。底层另有一组带编译期类型声明的总线原语(`RawQueue` 表示电平就绪、`RawPort` 表示边沿事件),早期基于它的唤醒通路正逐步并入这套以mailbox为后端的机制。整套语义对标 Linux 的 poll/epoll、`waitpid` 等行为,但实现是为本项目"步骤化执行 + 无锁观察"的模型量身设计的。

== 事务化变更

内核里一次"逻辑上的修改",落到内存里往往是好几步:往一张共享表里放一个新条目,要先确认这个键还没人占、再找个空位、再把键和值写进去。这几步如果不是一个不可分割的整体,就会出两类问题。一是改一半失败——写到中途因为表满了、键冲突了、或者错误沿着 `?` 提前返回了,表里就留下一个半初始化的残条目。二是占位竞态——两个 CPU 同时"查到空闲"然后各占一格。我们想要的是:对一张共享表的一组修改,要么全部生效,要么一点痕迹都不留。

=== 数据结构

承载这件事的是 `Index<K, V, N>`,一张定容量(N 是编译期常量)、自旋锁保护的并发索引。每个槽是一个四态小状态机,把"占位"和"可见"分成两个阶段:

```rust
const EMPTY: u8 = 0;              // 空
const RESERVED_EMPTY: u8 = 1;     // 已占位,还没写值——对读者不可见
const COMMITTED: u8 = 2;          // 已提交,可见
const RESERVED_COMMITTED: u8 = 3; // 正在改一个已存在的条目

pub struct Index<K, V, const N: usize> {
    lock: SpinLock,
    entries: [Entry<K, V>; N],    // 定容数组,无堆分配
}
```

改一张表分两步。`reserve(key)` 在锁里一次做完"查重 + 找空位 + 写键",把槽置成 `RESERVED_EMPTY`,返回一张凭证——这时槽已经被这次操作独占,但读者还看不见它(读者只认 `COMMITTED`)。确认无误后再 `commit(value)`,写值、翻成 `COMMITTED`,这才对外可见。

关键在于:凭证要是没提交就被丢掉(中途 `?` 返回、panic 展开),它的 `Drop` 会自动把槽退回 `EMPTY`、把键析构掉。回滚是编译器保证的,不靠手写 cleanup:

```rust
impl Drop for IndexReservation<'_, ..> {
    fn drop(&mut self) {
        if self.committed { return; }               // 已提交,不回滚
        if *state == RESERVED_EMPTY {               // 未提交:退回空槽、析构键
            key.assume_init_drop();
            *state = EMPTY;
        }
    }
}
```

读者那一侧,`lookup` 要求传入一个 epoch guard,而且只会看到 `COMMITTED` 的条目,中间态对它永远不可见。于是写一半的过程对读者是透明的:它要么看到旧值,要么看到新值,绝不会撞见半成品。

=== 用法

`mutation` 模块只是把这套动词包成对子系统更友好的薄封装(`install_if_absent` / `withdraw` / `swap`),并把错误对齐成子系统在用的 `MutationError`。真正能体现"事务化"的,是把多个修改绑在一起、要么全成。网络栈的连接表就是这么用的——插入一对连接时,先预订两个键,两个都订到了才一起提交:

```rust
let first  = self.connections.reserve(first_key)?;   // 占第一个键
let second = self.connections.reserve(second_key)?;  // 占第二个键;失败则 first 随返回 drop 回滚
first.commit(first_socket);
second.commit(second_socket);                         // 两个都订到,才一起提交
```

要是第二个 `reserve` 失败,`first` 随函数返回被 drop,第一个键自动退回空,绝不会留下"只插了半条连接"的状态。

这套"reserve → commit,没提交就 drop 回滚"其实是 substrate 反复用的同一个母题:对象池 Zone 分配对象时也是先 `reserve` 占槽、`sign` 才发布,凭证丢了同样自动回滚。无论是占一个对象的物理槽,还是占一张索引里的一个键,都用同一套线性凭证来保证"全成或不留痕"。底层的同步原语很朴素:一个不带毒化(内核不展开栈,无需 poison)的自旋锁 `SpinMutex`,默认零开销,需要时可打开锁竞争计时用于观测。

= 硬件抽象层

Txv2 实现了完整的硬件抽象层(Hardware Abstraction Layer,HAL),为 RISC-V64 和 LoongArch64 两种架构提供统一的、面向内核的接口。这些接口都与 CPU 架构相关,涵盖开机、页表、trap 与中断、时钟、用户内存读写等方面;设备驱动那一层的抽象不在其内。

平台选择上参考了 ArceOS/axHal 的静态思路:具体跑哪一套不在运行时决定,而是编译期就定死——内核对平台类型泛型,运行期没有"当前是哪个架构"的判断。各组件的具体做法,如惰性浮点保存、硬件 TLB 重填、用户指针缺页探测等,则是在调研 Chronix、Del0n1x 等同类内核后,按自己的接口重新实现的。

== 启动

内核启动从固件将控制权交给入口开始,依次经过四步:固件交接、`_start` 切换地址空间、跨平台外壳 `entry`、通用主线 `kernel_main`。

固件(RISC-V 为 OpenSBI,LoongArch 为 QEMU 直接引导)完成最底层初始化后,跳转到内核入口 `_start`,并传入核号与设备树/启动参数。

`_start` 把处理器从"物理地址、未分页"切换到"虚拟地址、内核运行于高半区"。内核镜像装载于物理低地址、却链接于高半区,因此切换前要先建立物理到虚拟的映射,而这一步两个架构各不相同:RISC-V 在物理内存中构建一张 Sv39 引导页表(临时恒等映射、物理内存直映射窗口、内核高半区映射三类),写入 `satp` 开启分页后跳转高半区;LoongArch 配置硬件的直接映射窗口(DMW),无需页表即可直接访问物理内存。

`_start` 随后跳转到 `rust_entry`,进入跨平台的 `entry`。`entry` 安装最小 trap 向量、把固件参数归一成统一的 `BootHandoff`、装好每核私有(per-CPU)指针,再调用内核主线;它不直接调用内核,而是通过 `KernelMain` 接口以泛型方式回调 `kernel_main`,具体连接由内核二进制提供,使 HAL 与内核互不依赖。

`kernel_main` 按依赖顺序初始化:先完成平台早期初始化,再启动 substrate(对象池、帧分配器、slab)使内核堆可用,随后建立完整 trap 向量、reactor 与各子系统,最后 exec 第一个用户程序 `/init` 进入用户态。

RISC-V 的启动代码主要是两段。`_start` 建立映射、切到高半区,最后跳进 Rust 入口:

```asm
_start:
    mv   s0, a0                        // 核号 hartid
    mv   s1, a1                        // 设备树 DTB 指针
    la   sp, __tx_boot_stack_top_load
    slli t1, s0, 17                    // 按 hartid 错开每核启动栈
    sub  sp, sp, t1
    // 清零 BSS;在 __bootstrap_root 写入恒等映射、直映射窗口、内核高半区三类映射
    srli t0, s2, 12
    li   t1, TX_RV64_SATP_SV39
    or   a0, t0, t1
    csrw satp, a0                      // 开启 Sv39 分页
    sfence.vma
    li   t0, TX_RV64_KERNEL_VIRT_OFFSET
    add  sp, sp, t0                    // sp / gp 移到高半区
    add  gp, gp, t0
    la   t1, __rust_entry_load
    add  t1, t1, t0
    jr   t1                            // 跳进高半区 rust_entry
```

进入 Rust 后,`entry` 做完每核公共收尾,再调用内核主线 `kernel_main`:

```rust
pub fn entry<P, K>(cpu_id: usize, firmware_arg: usize) -> !
where P: TxPlatform, K: KernelMain<P>
{
    P::install_minimal_trap_vector();
    let handoff = P::boot_handoff(cpu_id, firmware_arg);
    P::install_early_percpu(handoff.cpu_id);
    K::kernel_main(handoff)            // 回调进内核主线
}
```

== 页表管理

页表管理负责内核侧的硬件分页。它建立在一套固定的内核地址空间布局之上,向上为内核与各进程提供建立、修改映射的统一接口,向下把 RISC-V 与 LoongArch 两种差异显著的分页硬件收敛到同一接口之后。

=== 地址空间布局

虚拟地址空间以符号位划分为高、低两个半区。低半区自 0 起,是各进程私有的用户地址空间,进程间互不可见;高半区归内核所有,在所有进程间共享同一份映射。共享带来一个直接好处:进程切换时只需更换用户半区对应的页表,内核部分无须重建,也不会随进程数量增长而重复占用页表空间。

高半区主要容纳两部分。其一是直映射窗口,把整段物理内存按固定偏移连续映射到一段内核虚拟地址。其意义在于,内核运行中大量需要按物理地址访问内存——帧分配器刚分出的页要清零或写入、页表自身的中间级节点要读写、设备的物理内存区要访问——若每次都临时建立映射,代价高且易错;有了直映射窗口,任意物理地址加上固定偏移即得到一个可直接解引用的内核虚拟地址。其二是内核镜像映射,内核的代码段、只读数据段、数据段分别以可执行、只读、可读写的权限映射到高半区,使各段在硬件层面获得最小权限——代码段不可写,数据段不可执行。

两种架构采用相同的划分,仅具体地址不同。RISC-V 把高半区布局固化为一组常量,直映射窗口与物理地址只差一个固定偏移:

```rust
const DIRECT_MAP_BASE: usize = 0xffff_ffc0_0000_0000;        // 直映射窗口起点
const DIRECT_MAP_SIZE: usize = 128 * 1024 * 1024 * 1024;     // 覆盖 128 GiB
const KERNEL_VIRT_BASE: usize = 0xffff_ffff_8020_0000;       // 内核镜像起点

const fn direct_map_virt(phys: usize) -> usize {             // 物理地址 → 内核虚址
    DIRECT_MAP_BASE + phys
}
```

LoongArch 不为直映射单独建表,而是复用 DMW 硬件窗口直接覆盖全部物理地址(见后文),内核镜像也落在该窗口内。

#figure(
  ```text
        高地址
        ┌─────────────────────────────┐
        │  内核镜像                     │ ┐
        ├─────────────────────────────┤ │ 高半区(内核,所有进程共享)
        │  直映射窗口(整个物理内存)     │ ┘
        ╎              ……              ╎
        ├─────────────────────────────┤
        │  用户地址空间(每进程私有)     │   低半区(用户)
        └─────────────────────────────┘
        低地址(0)
  ```,
  caption: [内核虚拟地址空间布局],
)

=== 页表结构

两种架构开启分页、组织页表的硬件机制差异很大。

RISC-V 采用 Sv39:三级页表,39 位虚拟地址按 9 / 9 / 9 位依次索引三级目录,余下 12 位为页内偏移;每一级目录项既可指向下一级目录,也可直接作为叶子映射一个大页,因而天然支持 1 GiB、2 MiB、4 KiB 三种页面。将根页表的物理页号连同 Sv39 模式位写入 `satp` 寄存器即开启翻译;内核与用户共用一张根页表,高半区为内核条目、低半区为用户条目,切换地址空间只需改写 `satp`。页表项的低 8 位记录状态与权限,其中是否含读写执行位用于区分枝节点与叶子节点:

```rust
const PTE_V: u64 = 1 << 0; // 有效        const PTE_U: u64 = 1 << 4; // 用户可访问
const PTE_R: u64 = 1 << 1; // 读          const PTE_G: u64 = 1 << 5; // 全局
const PTE_W: u64 = 1 << 2; // 写          const PTE_A: u64 = 1 << 6; // 已访问
const PTE_X: u64 = 1 << 3; // 执行        const PTE_D: u64 = 1 << 7; // 已写脏

fn encode_leaf_pte(phys: PhysAddr, flags: u64) -> u64 {     // 物理页号 << 10 | 标志
    ((phys.0 as u64 >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D
}
fn pte_is_leaf(pte: u64) -> bool {                          // 带 R/W/X 即叶子
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) != 0
}
```

内核映射不置 `U`,进程映射置 `U`;而代码段、只读数据段、数据段三类内核映射的读写执行权限,则按链接期记录的段范围分别选取。

LoongArch 的页表层数与位宽并不写死,而是由 `PWCL`、`PWCH` 两个控制寄存器在启动时声明各级目录的起始位与宽度,本项目据此配置为四级、每级 9 位:

```rust
const fn la64_pwcl_value() -> usize {  // 页内 12 位;低三级目录从 bit 12 / 21 / 30 起、各 9 位
    12 | (9 << 5) | (21 << 10) | (9 << 15) | (30 << 20) | (9 << 25)
}
const fn la64_pwch_value() -> usize { 39 | (9 << 6) }  // 第四级目录从 bit 39 起、9 位
```

与 RISC-V 共用单张根不同,LoongArch 把用户半区与内核半区交给两张独立的根页表,分别由 `PGDL`、`PGDH` 指向,硬件依虚拟地址的最高位自动在两者间选择;因此切换地址空间只需更换 `PGDL`,常驻的内核根 `PGDH` 不动。分页由 `CRMD` 寄存器的分页位开启,页大小与重填入口另由 `STLBPS`、`TLBREHI` 设定。其页表项采用反向权限编码——页默认可读、可执行,以 `NR`、`NX` 位施加限制——并以存储访问类型位携带缓存属性、以特权级位区分内核页与用户页:

```rust
fn encode_la64_leaf_pte(phys: PhysAddr, perms: PmapPermissions) -> u64 {
    let mat = if perms.contains(DEVICE) { PTE_MAT_SUC } else { PTE_MAT_CC }; // 缓存属性
    let mut flags = PTE_V | PTE_A | PTE_PRESENT | mat;
    if !perms.contains(READ)    { flags |= PTE_NR; }            // 反向:默认可读
    if  perms.contains(WRITE)   { flags |= PTE_W | PTE_D; }
    if !perms.contains(EXECUTE) { flags |= PTE_NX; }            // 反向:默认可执行
    if  perms.contains(USER)    { flags |= PTE_PLV_USER; }      // 用户特权级
    (phys.0 as u64 & PFN_MASK) | flags
}
```

=== 映射的管理

页表建好之后,其中的映射在内核运行期间持续变动。启动阶段,内核要为自身建立必要的映射,并随探测到的物理内存把直映射窗口逐段扩展到位;运行阶段,每个进程的用户地址空间会随 `exec` 装载映像、`mmap` 申请区域、`fork` 复制空间、写时复制按需分页等操作不断地增加、修改与撤销映射。这两类改动——内核侧的共享映射,与每个进程私有根页表上的用户映射——都通过同一套 `PmapIf` 接口完成。

为防止在页表中留下建到一半的映射,接口把每次改动拆成预留与提交两个阶段:

```rust
pub trait PmapIf {
    fn reserve_kernel_mapping(virt, phys, kind)
        -> Result<Option<PmapReservation>, PmapError>;   // 预留,可失败
    fn commit_kernel_mapping(reservation, perms);         // 提交,不失败
    fn unmap_kernel_mapping(virt, kind);                  // 撤销 + TLB shootdown
    fn extend_direct_map(phys_end) -> Result<(), PmapError>;  // 扩展直映射窗口
    fn create_pmap_root() -> Result<PmapRoot, PmapError>; // 创建进程页表根
    fn activate_pmap(root: &PmapRoot);                    // 切换时激活
    // …
}
```

预留阶段按目标粒度(1 GiB、2 MiB、4 KiB)定位到对应的页表项;若通往该项的中间级目录尚不存在,则就地分配页表节点把缺失的层级补齐。这一阶段可能失败——没有空闲页表节点,或地址、长度未按粒度对齐——失败时本次新建的中间级目录被回滚归还,页表中不留任何半成结构;成功则返回一份凭证,其中记录本次涉及的中间级节点。提交阶段把叶子页表项连同权限写入,这一步不会失败。撤销映射时先清除页表项,再执行 TLB shootdown:通过核间中断令所有核刷新对应条目,确保没有任何核继续使用已失效的映射;仅修改权限时同理处理。

内核侧映射作用于共享的内核页表,进程映射作用于该进程的根页表,二者走同一套预留—提交流程,差别只在目标页表。进程根由 `create_pmap_root` 创建(创建时并入高半区的内核映射),并在上下文切换时激活——RISC-V 写 `satp`,LoongArch 则改写一组 CSR:

```rust
write_la64_csr(LA64_CSR_ASID, switch.asid);  // 关联地址空间标识
write_la64_csr(LA64_CSR_PGDL, switch.pgdl);  // 仅更换用户半区根
write_la64_csr(LA64_CSR_PGDH, switch.pgdh);  // 内核半区根(常驻不变)
write_la64_csr(LA64_CSR_CRMD, LA64_CRMD_PG | ...); // 开启分页
la64_invtlb_all();
```

两种架构都为根页表关联 ASID,使切换不必全量刷新 TLB,只淘汰其他地址空间的条目。支撑这一切的页表节点本身由一个固定容量的专用分配器供给,而非通用内核堆,从而避免在堆尚未建立的启动早期、以及对象回收路径上对堆产生依赖。

=== 直接映射与 TLB 重填

LoongArch 上有两处不经普通页表完成,这是它与 RISC-V 在分页上最大的不同。

其一是直接映射窗口。LoongArch 提供数个 DMW 控制寄存器,每个可配置一段虚拟地址窗口:当虚拟地址的高位与某窗口设定相符时,硬件直接取其低位作为物理地址,既不查页表、也不占用 TLB,窗口本身还指定该段的特权级与缓存属性。启动汇编一开始便配好两个窗口:

```asm
li.d  $t0, 0x9000000000000011   # 可缓存窗口:高 4 位=0x9,PLV0,可缓存
csrwr $t0, 0x180                # 写入 CSR.DMW0
li.d  $t0, 0x8000000000000001   # 非缓存窗口:高 4 位=0x8,PLV0,非缓存
csrwr $t0, 0x181                # 写入 CSR.DMW1
```

此后,内核按物理地址访问内存只需把高位或上对应基址即可,无需任何页表项:

```rust
const fn la64_cached_virt(phys: usize)   -> usize { 0x9000_0000_0000_0000 | phys } // 内存
const fn la64_uncached_virt(phys: usize) -> usize { 0x8000_0000_0000_0000 | phys } // 设备
```

可缓存窗口供内核自身执行以及按物理地址访问内存(即前述直映射窗口),非缓存窗口供访问设备寄存器。正因有 DMW,LoongArch 无须为内核镜像与直映射建立任何页表项,启动期也省去了 RISC-V 那样手工搭建引导页表的步骤。

其二是 TLB 缺失的处理。RISC-V 的 TLB 缺失由硬件页表遍历器自动重填,内核无须介入;LoongArch 则交给软件:TLB 未命中触发专门的重填异常,跳转到内核设置的重填入口,由内核以 `lddir`、`ldpte`、`tlbfill` 指令逐级遍历页表、取出页表项对并填入 TLB。该入口为尽快返回,只借用专用暂存寄存器,不构造完整的异常上下文。

== Trap 处理

trap 是控制流进入内核的统一入口,涵盖系统调用、缺页、中断与各类异常。HAL 在这里只承担"进出内核"的机械工作——保存现场、判断类型、跳到内核的对应处理、再按处理结果返回用户态;真正的处理逻辑在内核侧。

=== 处理流程

用户程序触发 trap 后,硬件跳到 HAL 预先安装的 trap 入口。入口先保存现场:切换到每核 trap 栈,把被陷线程的通用寄存器与关键 CSR 存入 trap 帧。随后判断 trap 类型(缺页、系统调用、定时器中断、外部中断、核间中断、非法指令等),据此回调内核侧对应的处理入口——缺页交给缺页处理、系统调用交给分发、外部中断则取号后分发给设备处理函数。

内核处理完并不直接返回,而是交回一个动作,由平台侧据此收尾。原因在于 trap 入口是一段同步代码,不能像执行器上的异步任务那样就地等待 I/O 或调度;因此处理函数把需要阻塞的工作交接给执行器,只回一个动作说明下一步:

- `Resume`:已就地处理完,恢复现场、直接返回用户态;
- `Reschedule`:需要阻塞或重新调度,退回异步执行器,就绪后再返回用户态;
- `DeliverSignal`:在用户栈布置信号现场,转入信号处理函数;
- `Terminate`:终止该进程。

这样,高频而廉价的 trap(如 `getpid`、空转的定时器中断、未唤醒任何线程的外部中断)走 `Resume` 当场返回,完全不经过异步执行器,省去一次调度往返;只有真正需要阻塞的 trap 才付出交接代价。处理入口与动作约定在 `KernelTrapSink` 中:

```rust
pub enum TrapAction { Resume, Reschedule, DeliverSignal, Terminate }

pub trait KernelTrapSink<P: TxPlatform> {
    fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction;
    fn on_syscall(view: TrapFrameMut<'_>) -> TrapAction;
    fn on_timer_interrupt(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction;
    fn on_external_irq(cpu: CpuId) -> TrapAction;
    fn on_ipi(cpu: CpuId) -> TrapAction;
    fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction;
}
```

以外部中断为例,处理函数取号、分发、应答后,只有当中断唤醒了等待的线程时才返回 `Reschedule`,否则 `Resume`:

```rust
fn on_external_irq(_cpu) -> TrapAction {
    let irq = P::claim();
    let handled = P::dispatch_irq(irq);
    P::complete(irq);
    match handled {
        IrqHandled::Wake => TrapAction::Reschedule, // 唤醒了等待线程 → 退回执行器
        _                => TrapAction::Resume,      // 没唤醒谁 → 直接返回
    }
}
```

=== 上下文的保存与恢复

进出内核都要保存与恢复被陷线程的通用寄存器和关键 CSR(两者是一对操作),但两种架构所用的暂存机制不同。

RISC-V 借助一个 `sscratch` 寄存器在用户栈与每核 trap 栈之间切换:陷入时 `sscratch` 持有 trap 栈顶,与 `sp` 一换即得到内核栈;返回时再换回。浮点寄存器按需保存——仅当 `sstatus.FS` 标记浮点被改动时才保存,省去多数 trap 的浮点开销。

```asm
addi  sp, sp, TX_RV64_TF_SIZE   # 释放 trap 帧
csrrw sp, sscratch, sp          # sp ↔ sscratch:换回用户栈
sret
```

LoongArch 没有等价的栈交换寄存器,改以多个 `KSAVE` 暂存寄存器分工:`KSAVE0` 预置 trap 栈顶、陷入时与 `sp` 交换,`KSAVE3` 暂存工作寄存器,`KSAVE1`、`KSAVE2` 保存内核线程指针类寄存器。保存完毕后,还要把内核入口地址转换为 DMW 缓存地址再跳入,以确保在缓存窗口中执行;返回用户态前则清除 `LLBCTL` 的链接位,以配合 ll/sc 原子序列的语义。

```asm
csrwr $sp, KSAVE0               # 用户 trap:换到每核 trap 栈,KSAVE0 存用户 sp
# …… 保存 GPR 与 ESTAT/ERA/BADV/CRMD/PRMD ……
li.d  $r13, 0x9000_0000_0000_0000   # 内核入口地址 → DMW 缓存地址
or    $r12, $r12, $r13
jirl  $r1, $r12, 0              # 进入内核处理
```

== 时钟与定时器

内核的时间和定时统一走 `TimeIf`——读当前时刻、安排下一次定时器中断:

```rust
pub trait TimeIf {
    fn read_ns() -> u64;          // 当前单调时间(纳秒)
    fn set_deadline_ns(ns: u64);  // 安排下一次定时器中断的绝对时刻
    fn cancel_deadline();
    fn frequency_hz() -> u64;     // 定时器频率
}
```

`read_ns` 读硬件单调计数器并换算成纳秒,`set_deadline_ns` 把"下一次该被打断的时刻"写进定时器。两架构实现不同:RISC-V 读 `time` CSR、经 SBI 的 `set_timer` 安排下一次中断;LoongArch 读稳定计数器、写定时器配置 CSR(`TCFG`)。reactor 的超时与定时就架在这之上——它把最近一个到期时刻交给 `set_deadline_ns`,定时器中断回来再推进时间轮。`read_ns` 还得足够便宜,因为它在调度、`clock_gettime` 这些热路径上被频繁调用。

== 用户态内存访问

内核经常要读写用户态指针(syscall 参数里的缓冲区、字符串等),但用户地址随时可能没映射或非法,直接解引用会让内核自己缺页崩掉。HAL 用一套"先试着访问、缺页了就优雅返回错误"的机制解决,思路同 Linux 的 `copy_from_user`。

以 RISC-V 为例:`copy_from_user` / `copy_to_user` 先打开 `sstatus.SUM`(允许内核态访问用户页),再跑一个裸的拷贝循环,循环里那条可能缺页的 load/store 被一对汇编标号"夹"起来,登记进一张 fixup 表:

```asm
tx_rv64_cfu_raw:                 // copy_from_user 字节循环
.Lcfu_loop:
tx_rv64_cfu_ld_s:  lbu t0, 0(a1) // ← 这条 load 落在 fixup 区间内
tx_rv64_cfu_ld_e:
    // … 正常路径:写入 dst、指针推进、循环 …
```

一旦这条 load 真的缺页,内核态缺页被 trap 分发发现 `sepc` 正落在 fixup 区间里,于是把 `sepc` 改写到恢复桩、把出错地址放进 `a0` 再返回——拷贝函数带着"出错地址非零"返回,Rust 包装层把它转成 `Err(FaultInfo)`。这样内核访问任何用户指针都不会被带崩,非法地址只会变成一个干净的 `EFAULT`。

== 其他硬件抽象

剩下几类架构相关的能力也各收在一个接口后面,内核按需调用:

- *中断控制器*(`IrqIf`):统一取号(`claim`)、应答(`complete`)与屏蔽。RISC-V 的 PLIC 是单级 MMIO 控制器;LoongArch 是 EIOINTC 叠加 PCH-PIC 两级,且两级访问通道不同——EIOINTC 走 IOCSR 指令、PCH-PIC 走非缓存 DMW,每次应答或屏蔽都要同时操作两级。
- *缓存与 DMA*(`CacheIf` / `DmaIf`):指令缓存同步(改了将要执行的代码后做 `fence.i`)、数据缓存的 clean / invalidate,以及物理地址与 DMA 地址互转、设备 DMA 前后的 `sync_for_device` / `sync_for_cpu`。QEMU 平台缓存一致,这些多为空实现,但接口给真机上的非一致 DMA 留好了位置。
- *浮点与 SIMD*(`FpSimdIf`):保存 / 恢复浮点上下文、按当前线程启用或关闭浮点。配合前面 Trap 一节的惰性保存——只有真用了浮点的线程才付保存恢复的代价。
- *LoongArch 非对齐访问*:LoongArch 对部分非对齐内存访问不由硬件兜底,而是触发异常。内核在异常里解码出错的那条指令(操作码表按 LoongArch ISA 整理),再用对齐方式把这次访问模拟完成,让用户程序无感。

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
