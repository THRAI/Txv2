# 系统调用模版介绍：以 `sys_read` 为例

> 面向演示 / 评审的讲解稿。所有代码片段与行号均取自当前工作树的真实实现，
> 非 `docs/Txv3/04_SYSCALL_SHAPE_v1.md` 中的理想化示例。设计文档描述的是
> 目标形态，本稿描述的是落地形态，并标注两者差异。

## 0. 为什么用 `read` 当主角

`read` 是最有说服力的演示对象，因为它一条调用就能层层揭示模版的全部性质：

- 它落在最重的派发车道（Lane 3，full async），能展示完整的 `drive` 驱动循环；
- 它的**上半**（签名解析）天然要面对十几种对象类型，能展示真实而非玩具的解析工作；
- 它的**下半**（payload transition）是一个对身份泛型的 `StepOp`，能在类型层面坐实
  “下半身份无关”这一核心卖点；
- 它每次调用都自动产出可观测 trace，能展示“模版红利”。

一句话定位：

> `read` 在本内核里不是一个函数，而是一个对身份泛型、可被驱动器反复推进的状态机：
> 上半把 `fd` 解析成带类型的能力，下半 `OpenFileReadOp<I>` 不关心权限归属、只管推进
> 字节，后端是 tty 还是 pipe 由 `step` 自己封装——整条调用自动产出可观测 trace。

---

## 1. 三条派发车道：`read` 落在哪一条

派发入口在 `crates/tx-shims/src/linux_syscall/mod.rs`。`dispatch_inner` 先走
Lane 1（Immediate，纯查询永不 yield），再走 Lane 2+3（脚本化）。`read` 在
Lane 3：

```rust
// mod.rs:965 — Lane 2+3 的 match
nr if nr == NR_READ => sys_read::<P>(req.args, ctx).await,
```

| 车道 | 进 StepOp？ | 进 drive？ | 可 yield？ | 代表 syscall | 真实代码锚点 |
|---|---|---|---|---|---|
| Lane 1 Immediate | 否 | 否 | 否 | `getpid` | `proc.rs:307` 直接 `Return(ctx.process.pid.0)` |
| Lane 2 OneShot | 是 | `drive_oneshot` | 否 | `setuid` / `exit` | `cred.rs:248` / `proc.rs:153` |
| Lane 3 Full async | 是 | `drive(...).await` | 是 | **`read`** / `write` / `futex` | `io.rs:2365` |

设计动机（`04_SYSCALL_SHAPE_v1.md §6.4`）：约 60% 已接线 syscall 永不 yield，
把它们全塞进 async 是浪费。车道用类型标记（`ImmediateSyscall` / `OneShotStepOp`
trait）在编译期约束——Lane 1 不准调 `drive`，Lane 2 不准返回 `Yield`。

> 演示对照：把 `getpid`（一行）、`setuid`（约 12 行，`drive_oneshot`）、`read`
> （`drive(...).await`）三段并排，说明“按需付费”——选哪条车道决定付多少代价。

---

## 2. 上半：把裸 `fd` 解析成带类型的能力

`sys_read` 入口（`crates/tx-shims/src/linux_syscall/io.rs:2365`）：

```rust
pub(super) async fn sys_read<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }

    // 上半:fd → Cap<OpenFile>。一切授权走 ctx,无全局 current()。
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // 随后按 backing 分派(io.rs:2385-2489):
    //   posix_mq / ufd / signalfd / eventfd / timerfd / socket /
    //   socketpair / zero 字符设备 / pipe / pagebacked / tty …
    // 各自走专门的 read 语义。
```

要点：

- **显式身份注入**：权限来自 `ctx.process`，没有任何全局 `current_thread()` /
  `current_cred()` 访问器（SUBJ-1 不变量）。
- **诚实交代**：真实上半带着十几个 backing 内联分支（`io.rs:2385-2489`），
  并不像设计文档示例那样“干净”。这是因为 Linux 的 `read` 本来就要面对这么多
  对象类型。模版的价值不在于消灭这些分支，而在于**分支之后**统一收束到下半的
  同一套 `StepOp` / `drive` 机制。

---

## 3. 下半：把“读”建模成可重入的状态机

`sys_read` 末尾对普通 VFS 文件构造下半 op 并交给 `drive`
（`io.rs:2532`）：

```rust
let op = OpenFileReadOp {
    file: &file,
    out: &mut staging,
    caller_netns: ctx.process.net_namespace(),
    cursor: 0,
};
match drive(
    op,
    &mut script_ctx,
    mode,                 // Waiting / Nonblocking — 阻塞与否只是一个参数
    mailbox_arc.as_ref(),
    delegate_registry_arc.as_deref(),
    timer_wheel_arc.as_ref(),
)
.await
{
    Ok(total) => {
        if total > 0 {
            // 累积完成后,一次性 copy 回用户空间
            if let Err(errno) =
                bootstrap_copy_to_user(&ctx.aspace, buf_ptr as u64, &staging[..total])
            {
                return SyscallResult::error_from(errno);
            }
        }
        SyscallResult::Return(total as i64)
    }
    Err(v3errno) => SyscallResult::error_from(v3errno.into()),
}
```

下半本体（`crates/tx-subsystems/src/vfs/execution.rs:845`）：

```rust
pub struct OpenFileReadOp<'a> {
    pub file: &'a Cap<OpenFile>,
    pub out: &'a mut [u8],
    pub caller_netns: Option<PayloadCap<NetNamespacePayload>>,
    /// 内部写游标:每次 step() 从 out[cursor..] 起填充,并推进 cursor。
    /// drive() 据此反复调用 step() 而无需调用方在迭代间更新 out(DRIVE-2)。
    pub cursor: usize,
}

impl<'a, I: SubjectIdentity> StepOp<I> for OpenFileReadOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<usize, ByteProgress> {
        let guard = step_engine::guard();          // guard 在 step 内取,绝不跨 .await
        let caller_netns = self.caller_netns.as_ref().map(|n| &**n as &NetNamespacePayload);
        let result =
            self.file.step_read_with_netns(&mut self.out[self.cursor..], caller_netns, &guard);
        match &result {
            StepOutcome::Done(n)          => self.cursor += *n,
            StepOutcome::Continue { progress } => self.cursor += progress.bytes(),
            StepOutcome::Yield { progress, .. } => self.cursor += progress.bytes(),
            StepOutcome::Err(_)           => {}
        }
        result
    }
}
```

要点：

- **读是状态机，不是一次性函数**。内部 `cursor` 记录进度，`drive` 负责“喂”它：
  数据没齐就 park 到 wait-source，有进展就推进，直到 `Done`。
- **阻塞与非阻塞只是 `mode` 参数差异**（`DriveMode::Waiting` vs `Nonblocking`），
  下半代码完全相同。
- **EBR 纪律**：epoch `guard` 在 `step()` 内部获取，绝不跨 `.await`
  （`STEP_MODEL_v2 §1`，`INVARIANTS_v5` YIELD-5 / EBR-7）。`Cap<T>` 可跨 await，
  guard 不行。

---

## 4. 核心卖点：下半在类型层面与身份解耦

注意上面 `impl` 的签名：

```rust
impl<'a, I: SubjectIdentity> StepOp<I> for OpenFileReadOp<'a> { … }
```

`OpenFileReadOp` 对身份类型 `I` 是**泛型**的——它不知道自己跑在谁的权限下。

- native syscall 路径用 `ProcessIdentity` 实例化
  （`crates/tx-substrate/src/step/subject_context.rs:169`：
  `impl SubjectIdentity for ProcessIdentity`）。
- 将来 io_uring / AIO 的代办（OnBehalfOf）身份，只要实现同一个 `SubjectIdentity`
  trait，**这份下半代码一行不改即可复用**。

> 诚实边界（务必在演示中讲清，否则会被懂行的人当场拆穿）：
>
> - `SubjectIdentity` 目前**只有 `ProcessIdentity` 一个实现**。所以“native 与
>   OnBehalfOf 复用同一下半”在 `read` 上是**类型已就绪、第二个身份类型尚未落地**。
> - io_uring 当前是 scaffold：`io_uring.rs` 的 `sys_io_uring_enter` 只是 pop 一个
>   SQE、push 一个零结果 CQE，**并未**真正复用 `sys_read` 的下半。
> - AIO（`aio.rs`）另写了 `dispatch_pread` / `dispatch_pwrite`，复用的是
>   `with_on_behalf_of` 借用身份的**纪律与授权 helper**（`process.fd(...)`、
>   `bootstrap_copy_to_user(aspace, ...)`），而非字面复用 `OpenFileReadOp`。
>
> 正确表述：**这条复用路径已经在类型系统里焊死，是既定方向；统一到字面复用
> 仍在推进中。** 不要说“现在 io_uring 就在复用 read”。

---

## 5. “the read is a read”：后端多态被封装

具体读哪种后端，由下半内部的 `step_read_with_netns` 按 backing 分派
（`crates/tx-subsystems/src/vfs/execution.rs:379`）：

```rust
match self.rnode().backing() {
    RNodeBacking::StructBacked { payload } => match payload {
        StructPayload::Tty(tty)       => tty::execution::step_read(tty, out, guard),
        StructPayload::CharDevice(b)  => b.ops.read(out, guard),
        StructPayload::BlockDevice(_) => StepOutcome::Err(Errno::ENOSYS),
        StructPayload::Pipe { .. }    => /* pipe 读语义 */,
        StructPayload::Socket { .. } | StructPayload::NetNamespace { .. }
            | StructPayload::MountNamespace { .. } => StepOutcome::Err(Errno::EINVAL),
        // …
    },
    RNodeBacking::Directory => StepOutcome::Err(Errno::EISDIR),
    // …
}
```

要点：对 `drive` 和上半脚本来说，“读就是读”。tty 读会 park 在终端可读事件上、
pipe 读 park 在管道事件上、普通文件直接返回——产出的 `YieldShape` 不同，但驱动
循环按形态各自处理，脚本完全无感。内核选哪种 yield 原语被封装在 `step` 内部。

---

## 6. 模版红利：可观测 trace 是免费的

派发入口在调用 `sys_read` 前后自动开关一条 L0 boundary span，并为每个寄存器参数
a0–a5 发一条 `ArgValue` 记录（`crates/tx-shims/src/linux_syscall/mod.rs:517`，
`emit_syscall_enter` / `emit_syscall_exit`）：

```rust
pub async fn dispatch<'a, P: …>(req: SyscallRequest, ctx: &SyscallCtx<'a>) -> SyscallResult {
    let l0_span = emit_syscall_enter(&req);                 // 自动开 span + 发 a0..a5
    let prev = tx_observe::set_current_parent_span(l0_span);
    time::poll_itimer_real_on_syscall_boundary::<P>(ctx);
    let result = dispatch_inner::<P>(req, ctx).await;        // 内部调用 sys_read
    tx_observe::set_current_parent_span(prev);
    emit_syscall_exit(l0_span, &result);                    // 自动关 span,带返回值
    result
}
```

你写 `sys_read`，不加任何埋点，就自动得到一条带参数标注（fd / buf / len）的
Perfetto trace 切片，并通过 `set_current_parent_span` 与下层 L2（`drive`）/ L4 span
形成父子链。这是最具体、最难反驳的“模版红利”。

> 演示素材：`cargo xtask test smoke` 或 `cargo xtask qemu` 抓 serial 日志，
> 展示真实 `sys_read` 切片。

---

## 7. 演示编排速查（5 屏）

| 屏 | 主题 | 真实锚点 | 一句话 |
|---|---|---|---|
| 1 | 三车道，`read` 在 Lane 3 | `mod.rs:965` | 选哪条车道决定付多少代价 |
| 2 | 上半:fd → `Cap<OpenFile>` | `io.rs:2365-2489` | 显式身份,无全局 `current()` |
| 3 | 下半:可重入状态机 | `io.rs:2532` + `execution.rs:845` | 读是状态机,阻塞只是 mode 参数 |
| 4 | 身份无关(类型证据) | `execution.rs:857` + `subject_context.rs:169` | `StepOp<I>` 泛型焊死复用路径 |
| 5 | “read is a read” + 免费 trace | `execution.rs:379` + `mod.rs:517` | 后端多态被封装;trace 自动产出 |

---

## 附:行号索引（便于现场点开）

- `crates/tx-shims/src/linux_syscall/mod.rs:517` — `dispatch`：L0 span 自动开关
- `crates/tx-shims/src/linux_syscall/mod.rs:965` — `NR_READ` 派发到 Lane 3
- `crates/tx-shims/src/linux_syscall/io.rs:2365` — `sys_read` 入口（上半）
- `crates/tx-shims/src/linux_syscall/io.rs:2385-2489` — 上半 backing 分派
- `crates/tx-shims/src/linux_syscall/io.rs:2532` — 构造 `OpenFileReadOp` 并 `drive`
- `crates/tx-subsystems/src/vfs/execution.rs:845` — `OpenFileReadOp` 定义
- `crates/tx-subsystems/src/vfs/execution.rs:857` — `impl<I: SubjectIdentity> StepOp<I>`
- `crates/tx-subsystems/src/vfs/execution.rs:379` — `step_read_with_netns` 后端 match
- `crates/tx-substrate/src/step/subject_context.rs:169` — 唯一的 `SubjectIdentity` 实现
- 对照例：`proc.rs:307`（getpid / Lane 1）、`cred.rs:248`（setuid / Lane 2）

> 文档与实现版本基准：当前分支 `codex/test-remote-network` 工作树快照。
> 行号可能随后续改动漂移，引用前建议用 `grep -n` 复核函数名。
