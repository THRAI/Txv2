# E2E Architecture Gap Closure — Three-Phase Plan

**Status:** proposed.
**Date:** 2026-05-14.
**Scope:** Close the remaining gaps between current implementation and the
architectural-completeness E2E target defined by `docs/Txv3/01_CONCEPTS_v5.md`
(five primitive cells, seven-layer architecture, per-subsystem step catalogs).

**Predecessor:** BLAST_RADIUS v3 migration (ADV
`docs/progress/decisions/2026-05-14-blast-radius-step-v3-migration.md`).

## 1. Current State

The BLAST_RADIUS migration completed six axes: v4 vocabulary retired,
`StepOutcome` narrowed to four variants, 69 `step_*` functions wrapped as
`StepOp`, `bus/` Waker→TaskMailbox, `step_v3/` scaffold populated, and v4→v5
deprecation annotations applied. 957 tests pass across four crates.

The remaining gap per `01_CONCEPTS_v5.md` is the **Publication loop closure**
and **reserved catalog population**:

```
Publication 闭环
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
① 语义对象状态迁移 (zone commit)          ✅ 所有子系统
② bus fire (RawQueue/RawPort)            ✅ bus 已迁移到 TaskMailbox
③ TaskMailbox post (MailboxEvent)        ✅ mailbox 基础设施就绪
④ reactor wake (waker → re-poll)         ✅ WaitFuture 已适配
⑤ drive() yield resolution               ✅ OnWaitSource/OnAgent/OnTimer 已接线
⑥ 调用者重试 (apply_resume → step())     ✅ drive() loop 完整
════════════════════════════════════════════════════
⚠  PageBacked 的 Publication ⑥→① 闭环：read/write 手动循环
   未迁移到 drive() — 缺 PartialReturn (StepProgress::into_output)
⚠  epoll 的 OnEdge Publication：catalog 预留，实现延期
```

## 2. Design References

| Layer | Document | Role in this plan |
|---|---|---|
| Algebra | `docs/Txv3/03_STEP_MODEL_v2.md` §5–6 | YieldShape catalog, StepProgress monoid, PartialReturn |
| Protocol | `docs/Txv3/03_STEP_MODEL_v2.md` §7 | DriveMode::Nonblocking → AcceptOutcome::Translate(PartialReturn) |
| Composition | `docs/Txv3/04_SYSCALL_SHAPE_v1.md` | Upper/lower script split, SubjectContext |
| Semantic | `docs/design/03_memory-vm/PAGE_BACKED_v1.md` | PageBacked read/write step catalog |
| Identity | `docs/Txv3/06_EXECUTION_SCOPE_v1.md` | ExecutionScope, OnBehalfOf |
| Invariants | `docs/Txv3/02_INVARIANTS_v5.md` | STEP-3 (progress monotonicity), ASYNC-2 (yield boundary) |

## 3. Phase A — Publication 闭环（~300 lines）

**Goal:** `sys_read` / `sys_write` 迁移到 `drive()`，消除手动 StepOutcome 匹配循环。
**Prerequisite:** `StepProgress::into_output` — 允许 `drive()` 在 `PartialReturn`
模式下返回已累积的 progress。

### A.1 — StepProgress::into_output

**File:** `crates/tx-substrate/src/step/mod.rs`

```rust
/// Extract accumulated progress as an output value.
///
/// Used by `drive()` when `DriveMode::Nonblocking` produces a
/// `PartialReturn` translation: the driver has accumulated progress
/// across one or more `Continue` steps, and now must surface that
/// partial work to the caller.
fn into_output(self) -> Option<T>;
```

**Design constraint (STEP-3):** `into_output` is only valid for `StepProgress`
types that carry a `T` payload (e.g., `ByteProgress(usize)`). The `NoProgress`
type returns `None` — no partial return semantics.

**Verification:**
- `cargo test -p tx-substrate --lib -- step_progress_monoid`
- Lint: `cargo xtask lint invariants all` remains ceiling 0

### A.2 — drive() PartialReturn handling

**File:** `crates/tx-scripts/src/drive.rs`

Current `PartialReturn` arm returns `Err(EAGAIN)` with no output:

```rust
AcceptOutcome::Translate(Translation::PartialReturn) => {
    return Err(Errno::EAGAIN);
}
```

New: call `progress.into_output()` — if `Some(val)`, return `Ok(val)` (partial
success). If `None` (no progress accumulated), return `Err(EAGAIN)`.

**Verification:**
- `cargo test -p tx-scripts --test drive` — add `drive_partial_return_surfaces_accumulated_progress`
- `cargo test -p tx-subsystems --lib` — 623 pass unchanged

### A.3 — sys_read → drive()

**File:** `crates/tx-shims/src/linux_syscall/io.rs`

Current `sys_read` uses a manual loop:

```rust
loop {
    match op.step(&mut ctx) {
        StepOutcome::Done(read) => { copy_to_user; return Ok(read) }
        StepOutcome::Continue { progress } => { copy_to_user; accumulate; continue }
        StepOutcome::Yield { progress, shape: OnWaitSource { source, interests } } => {
            copy_to_user; park_on(source, interests).await; continue
        }
        StepOutcome::Err(e) => return Err(e)
        _ => return Err(EAGAIN)
    }
}
```

Replace with `drive()` + user-buffer copy wrapper:

```rust
let total = drive(op, &mut ctx, DriveMode::Waiting, mailbox, None, None).await?;
bootstrap_copy_to_user(&ctx.aspace, buf, &staging[..total])?;
return Ok(total)
```

**Blocking path** (`O_NONBLOCK` not set): use `DriveMode::Waiting` — parks on
yield until `Done`.

**Nonblocking path** (`O_NONBLOCK` set): use `DriveMode::Nonblocking` — after
Phase A.2, returns partial progress instead of EAGAIN.

**Verification:**
- `cargo test -p tx-shims --lib` — fd_ops_wave* tests pass
- `cargo xtask lint invariants no-adhoc-drive` — ad-hoc site count decreases

### A.4 — sys_write → drive()

**File:** `crates/tx-shims/src/linux_syscall/io.rs`

Same pattern as sys_read. Mirror of A.3.

### A.5 — F_SETFL (fcntl)

**File:** `crates/tx-shims/src/linux_syscall/fs_basic.rs`

Wire `F_SETFL` to update `OpenFile.flags` with `O_NONBLOCK` / `O_APPEND` /
`O_DIRECT` etc. This is the gate that lets userspace toggle nonblocking mode
on existing fds.

**Verification:**
- `cargo test -p tx-shims --lib -- fcntl_setfl`

### Phase A Verification Gate

```
cargo xtask lint invariants no-adhoc-drive     → ad-hoc sites: 0 (ceiling 0)
cargo test -p tx-scripts --test drive           → all pass
cargo test -p tx-shims --lib                     → all pass
cargo test -p tx-subsystems --lib                → 623 pass
```

## 4. Phase B — Reserved Catalog 填充（~400 lines）

**Goal:** `epoll` (OnEdge YieldShape) 和 `userfaultfd` (OnAgent 闭环) 达到
catalog 定义的完整形态。

### B.1 — epoll_create1 / epoll_ctl / epoll_wait

**YieldShape:** `OnEdge` — deferred in v3, now implemented.

**Files:**
- `crates/tx-substrate/src/step/mod.rs` — add `YieldShape::OnEdge { source: WaitSourceId, interests: InterestMask }`
- `crates/tx-shims/src/linux_syscall/epoll.rs` (new) — epoll_* syscall arms
- `crates/tx-subsystems/src/epoll/` (new) — EpollIdentity / EpollPayload

**Design:** epoll is a readiness-aggregation layer over bus subscriptions.
`epoll_ctl(ADD)` subscribes to the target fd's `WaitSource` with a
per-epoll-instance `InterestMask`. `epoll_wait` parks on an `OnEdge` yield and
scans ready subscriptions.

**Reference:** `docs/Txv3/03_STEP_MODEL_v2.md` §5 — `YieldShape::OnEdge`
placeholder.

**Verification:**
- `cargo test -p tx-shims --lib -- epoll`
- Integration test: epoll-wait on pipe readability

### B.2 — userfaultfd OnAgent E2E

**YieldShape:** `OnAgent` — already wired in `drive()`. The gap is the
fault-interception path: when a VM fault occurs on a userfaultfd-registered
VMA, the fault handler must produce an `OnAgent` yield with the fault message.

**Files:**
- `crates/tx-subsystems/src/userfaultfd/mod.rs` — `step_ufd_read` already wired
- `crates/tx-subsystems/src/vm/execution.rs` — fault handler: check for
  userfaultfd registration, enqueue `uffd_msg`, fire wait source

**Design (per `docs/Txv3/05_DELEGATE_v1.md` §8.1):**
1. Fault occurs → `fault_script()` checks range against userfaultfd registrations
2. If registered → enqueue fault message into `UserfaultFd.pending_faults`
3. Fire `ufd.wait_source` → wakes `sys_read` on ufd
4. Monitor thread's `sys_read` returns `uffd_msg` → userspace handles fault
5. Userspace calls `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` → kernel resolves fault

**Verification:**
- `cargo test -p tx-subsystems --test v3_userfaultfd_fd_scaffold`
- `cargo test -p tx-subsystems --test v3_userfaultfd_ioctl_reply`
- `cargo test -p tx-subsystems --test v3_userfaultfd_register`

### Phase B Verification Gate

```
cargo test -p tx-shims --lib -- epoll                         → pass
cargo test -p tx-subsystems --test v3_userfaultfd_*            → pass
cargo xtask lint invariants all                                 → ceiling 0
```

## 5. Phase C — 语义补全（~500 lines）

**Goal:** 补齐 catalog 中设计文档已定义但实现延期的语义操作。

### C.1 — Mount Namespace 视图

**Reference:** `docs/design/05_filesystem/MOUNT_v1.md`

**Files:**
- `crates/tx-subsystems/src/mount/namespace.rs` (new) — MountNamespace entity
- `crates/tx-subsystems/src/process/execution.rs` — `ProcessPayload` carries
  `Shared<MountNamespace>`

**Operations:** `pivot_root`, `chroot`-like mount-namespace clone on `clone(CLONE_NEWNS)`.

### C.2 — Cred capset

**Reference:** `docs/design/02_execution/cred_service_v_1_draft (2).md`

**Files:**
- `crates/tx-subsystems/src/cred/mod.rs` — add `step_capset`

**Operations:** `capget` / `capset` — Linux capability bounding set manipulation.

### C.3 — AIO io_uring 基础路径

**Reference:** `docs/Txv3/05_DELEGATE_v1.md` §8.2

**Files:**
- `crates/tx-shims/src/linux_syscall/io_uring.rs` (expand) — `io_uring_enter` +
  `io_uring_register`
- `crates/tx-subsystems/src/io_uring/` (new) — IoUringIdentity / IoUringPayload

**Scope:** Phase C only targets the `OnAgent` submission path (SQPOLL deferred).
`io_uring_enter` accepts SQEs, produces `OnAgent` yields to the worker thread.

### Phase C Verification Gate

```
cargo test -p tx-subsystems --lib                                 → all pass
cargo test -p tx-shims --lib                                       → all pass
cargo xtask lint invariants all                                     → ceiling 0
```

## 6. Cross-Phase Risk Register

| Risk | Phase | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| `PartialReturn` semantics break nonblocking I/O edge cases | A | Medium | High | Proptest for ByteProgress monoid + Nonblocking mode matrix |
| `sys_read` user-buffer copy interleaving with drive() is incorrect | A | Medium | High | Bootstrap copy before drive() call; test with multi-page reads |
| epoll readiness semantics mismatch Linux edge/level behavior | B | Medium | Medium | Table-driven test per `epoll(7)` man page scenarios |
| userfaultfd fault-interception path creates deadlock under RangeLock contention | B | Low | High | Test concurrent fault + munmap on same VMA |
| Mount namespace clone conflicts with existing Shared<MountNamespace> lifetime | C | Low | Medium | Pre-audit Shared<T> drop semantics on clone |
| io_uring enter path conflicts with existing AIO scaffolding | C | Medium | Low | Reuse AioContext zone; separate sqpoll to follow-on PR |

## 7. Estimated Effort

| Phase | Lines | New files | Modified files | Wall time |
|---|---|---|---|---|
| A — Publication loop | ~300 | 0 | 3 (step/mod.rs, drive.rs, io.rs, fs_basic.rs) | 2–3 sessions |
| B — Catalog fill | ~400 | 2 (epoll.rs, epoll/) | 3 (step/mod.rs, vm/execution.rs, ufd/mod.rs) | 3–4 sessions |
| C — Semantic fill | ~500 | 3 (namespace.rs, io_uring/, epoll/) | 4 (cred, process, mount, io_uring) | 3–4 sessions |
| **Total** | **~1200** | **5** | **10** | **8–11 sessions** |

## 8. Dependency Order

```
Phase A ──────────────┐
  (PartialReturn,      │
   drive() migration,  │
   F_SETFL)            │
                       ├──→ Phase B ──→ Phase C
                       │     (epoll,      (mount ns,
                       │      ufd E2E)     cred capset,
                       │                   aio uring)
                       │
```

Phase A is strictly prerequisite: without `PartialReturn`, `sys_read` cannot use
`drive()` for nonblocking fds, and epoll/userfaultfd/AIO all depend on correct
nonblocking I/O semantics.

Phase B and C can partially overlap once Phase A lands — epoll and userfaultfd
are independent enough to develop in parallel.

## 9. Next Action

Start Phase A.1 — `StepProgress::into_output`:

1. Read `docs/Txv3/03_STEP_MODEL_v2.md` §6 for `StepProgress` trait definition
2. Read `crates/tx-substrate/src/step/mod.rs` for current `StepProgress` impls
3. Add `fn into_output(self) -> Option<T>` to the trait
4. Implement for `ByteProgress(usize)` → `Some(self.0)`
5. Implement for `NoProgress` → `None`
6. Wire into `drive.rs` `PartialReturn` arm
7. Test: partial-return + nonblocking read scenarios
