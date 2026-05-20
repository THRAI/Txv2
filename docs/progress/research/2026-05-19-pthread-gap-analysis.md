---
date: 2026-05-19
topic: "pthread_create 底层依赖缺口分析"
status: complete
prior:
  - docs/progress/research/2026-05-06-fork-clone-wait4-scaffolding.md
  - docs/progress/plans/2026-05-06-fork-clone-wait4.md
  - docs/progress/plans/2026-05-07-shell-prompt-roadmap.md
---

# pthread_create 底层依赖缺口分析

## 背景

2026-05-06 的 fork/clone/wait4 研究笔记已将 pthread_create 标记为"deferred"
——需要 `Shared<T>` 基础设施 + `step_clone_thread` + futex 唤醒。本笔记量化具体
缺口，为实现计划提供依据。

## 当前代码状态

### 已有

- `step_fork`：支持 `SIGCHLD`、`CLONE_VM`、`CLONE_VFORK`、`CLONE_SETTLS` 标志位。
  `clone_vm` 路径做 `parent_aspace.clone()`（Cap 克隆），非共享。
- `futex`：`FUTEX_WAIT` / `FUTEX_WAKE` 已接线，支撑 musl libc 的 pthread_once guard。
- `set_tid_address`：存储 `clear_child_tid` 指针，不做线程退出时 futex 唤醒。
- `set_robust_list`：返回 0 的 stub。
- `exit` / `exit_group` / `gettid` / `getpid`：已实现。
- 信号子系统：`sig_actions` 表、`group_pending`、信号投递已完成。
- `ProcessPayload` 字段：`aspace: AtomicSlot<Cap<AddressSpace>>`、`threads`、
  `sig_actions: SigActionTable`、`cred`、`cwd` 等。

### 显式标记 TODO/deferred

| 缺口 | 标记位置 |
|---|---|
| `Shared<T>` 基础设施 | 代码中完全不存在；fork-clone-wait4 研究笔记定论"entirely missing today" |
| `CLONE_THREAD` / `CLONE_SIGHAND` / `CLONE_FILES` / `CLONE_FS` | syscall 层 `-EINVAL` 拒绝 (`proc.rs:256`) |
| `step_clone_thread` | `PROCESS_v1` §7.1.3 已指定但仅在 `execution.rs:319` 有声明桩 |
| `clear_child_tid` futex 唤醒 | `TODO(phase-tls)` (`proc.rs:654`) |
| TLS setup（CLONE_SETTLS）| 标志位接受但实际语义不完整 |
| vfork / posix_spawn | 延期 |

## pthread_create 完整依赖链

musl 的 `pthread_create` 发出：

```
clone(CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD |
      CLONE_SYSVSEM | CLONE_SETTLS | CLONE_PARENT_SETTID |
      CLONE_CHILD_CLEARTID, stack, ptid, tls, ctid)
```

需要底层支撑：

| 依赖 | 当前状态 | 阻塞程度 |
|---|---|---|
| `Shared<T>` 基础设施 | 不存在 | **根本阻塞** |
| `step_clone_thread`（同一进程内创建新线程）| 不存在 | **根本阻塞** |
| `CLONE_VM` — 共享 AddressSpace | 有 `clone_vm: bool` 参数但做 Cap 克隆而非共享 | 依赖 Shared<T> |
| `CLONE_FILES` — 共享 fd table | fd table 尚未上 ProcessPayload | 依赖 Shared<T> |
| `CLONE_SIGHAND` — 共享 sig_actions | `sig_actions` 是值类型，不可共享 | 依赖 Shared<T> |
| `CLONE_FS` — 共享 fs_context | cwd 是独立 Cap，无共享模型 | 依赖 Shared<T> |
| `CLONE_SETTLS` — TLS 指针设置 | 标志接受，实际语义待确认 | 中等 |
| `CLONE_CHILD_CLEARTID` + futex 唤醒 | 指针存了但不做唤醒 | 阻塞 pthread_join |
| `CLONE_PARENT_SETTID` | 未实现 | 中等 |
| `set_robust_list` 真实实现 | stub | 阻塞 robust mutex |
| 线程退出时 clear_child_tid futex 写零+唤醒 | TODO | 阻塞 pthread_join |
| pthread_join 的 futex wait | 需要完整 futex 支持 | 阻塞 |

## 核心缺口详解：Shared<T> + Frame

`PROCESS_v1` §3 要求 ProcessPayload 上有 Frame：

```rust
pub struct Frame {
    pub vm: Shared<AddressSpace>,         // CLONE_VM
    pub fd_table: Shared<FdTable>,        // CLONE_FILES
    pub sig_actions: Shared<SigActionTable>, // CLONE_SIGHAND
    pub fs_context: Shared<FsContext>,    // CLONE_FS
    pub cwd: Cap<DEntry>,
    pub root: Cap<DEntry>,
}
```

当前代码中这些是 ProcessPayload 上的独立字段（`aspace: AtomicSlot<Cap<AddressSpace>>`、
`sig_actions: SigActionTable` 等），没有 Shared<T> 包装。

Shared<T> 需要四个 API（§3.1）：

- `share()` — 增加引用计数，返回指向同一 T 的 Shared<T>
- `fork_copy()` — COW 语义
- `get()` — 只读访问
- `get_mut()` — 可写访问，refcount > 1 时触发 COW

## 结论

**底层依赖不够。** 离 pthread_create 还差一个完整 slice，规模和 trio（fork/clone/wait4）相当。

实现需要按顺序落地：

1. `Shared<T>` 基础设施 — 所有共享语义的前提
2. Frame 重构 ProcessPayload
3. `step_clone_thread` 实现 — 同进程内创建 ThreadIdentity
4. CLONE_* 标志位 clone 路径
5. CLONE_SETTLS / CLONE_PARENT_SETTID / CLONE_CHILD_CLEARTID
6. 线程退出时 clear_child_tid futex 唤醒
7. set_robust_list 真实实现

总估计：~1,480 LOC，8-12 天。零框架变更。
