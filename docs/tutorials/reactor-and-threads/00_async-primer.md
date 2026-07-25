# Part 0 — A Primer: Futures as State Machines

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Next:** [Part 1 — The Reactor as a Scheduler](01_reactor-as-scheduler.md)

If you already think in `poll`/`Waker` terms, skim the summary at the end and move
on. Otherwise, this chapter gives you exactly the async vocabulary the rest of the
series leans on — no more.

## The pull model in one idea

A traditional kernel thread is *push-driven*: the CPU pushes instructions through
it until something forces it to stop (a trap, a `schedule()` call). Its progress
lives implicitly in a kernel stack and a saved register set.

A Rust future is *pull-driven*: it makes progress only when someone calls `poll`
on it. Between polls it does nothing and consumes no CPU. Its progress lives
explicitly in a value — a struct the compiler generates from your `async` code.

That single inversion — *progress lives in a value you can poll, not in a stack
you must switch to* — is the whole reason this kernel can suspend a thread without
parking a stack.

## The `Future` contract

This is the real trait from `core` (the kernel is `no_std`, but this trait lives
in `core`, so it is the same one you would use anywhere):

```rust
pub trait Future {
    type Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}

pub enum Poll<T> {
    Ready(T),
    Pending,
}
```

Three things to internalize:

1. **`poll` returns immediately.** It either produces `Ready(value)` (the future
   is done) or `Pending` (not done yet — try again later). It never blocks the
   thread it runs on.
2. **The future owns its progress.** Each call to `poll` resumes from wherever the
   last one left off. Where does it store "where it left off"? See the state
   machine below.
3. **`Context` carries a `Waker`.** When a future returns `Pending`, it is making
   a promise: "I have stashed the `Waker` from `cx`; I will call it when it is
   worth polling me again." The executor relies on that promise to avoid spinning.

## `async fn` *is* a state machine

When you write `async fn`, the compiler rewrites it into a struct that implements
`Future`. Each `.await` point becomes a *state*. The struct's fields are the local
variables that must survive across an `.await`.

Take this async function:

```rust
async fn copy_one(src: &Pipe, dst: &Pipe) -> usize {
    let byte = src.read_one().await;   // await point A
    dst.write_one(byte).await;         // await point B
    1
}
```

Conceptually the compiler turns it into something like:

```rust
enum CopyOne {
    Start { src, dst },
    AfterRead { dst, read_fut: ReadOne },   // suspended at A
    AfterWrite { write_fut: WriteOne },      // suspended at B
    Done,
}

impl Future for CopyOne {
    type Output = usize;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<usize> {
        loop {
            match self.state {
                Start { .. }      => { /* begin read; -> AfterRead */ }
                AfterRead { .. }  => match read_fut.poll(cx) {
                    Pending      => return Poll::Pending,   // still waiting at A
                    Ready(byte)  => { /* begin write; -> AfterWrite */ }
                },
                AfterWrite { .. } => match write_fut.poll(cx) {
                    Pending      => return Poll::Pending,   // still waiting at B
                    Ready(())    => { self.state = Done; return Poll::Ready(1); }
                },
                Done => unreachable!(),
            }
        }
    }
}
```

The point is not the exact shape — it is this: **the "saved context" of an async
computation is just the fields of a struct.** No stack is involved. Suspending is
returning `Pending`; resuming is being polled again and matching on the saved
state. Hold onto this — in [Part 2](02_thread-as-a-future.md) the "thread" whose
context we save across a syscall is exactly this kind of struct.

## `Waker`: the wake-up call

A future that returns `Pending` must arrange to be polled again later. It does so
through the `Waker` it pulls out of `cx`:

```rust
fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<T> {
    if self.ready() {
        Poll::Ready(self.take())
    } else {
        self.stash_waker(cx.waker().clone());  // "call me when ready"
        Poll::Pending
    }
}
```

Later, some *other* code — an interrupt handler, a timer, another task — calls
`waker.wake()`. That does not run the future. It just tells the executor "this
future is worth polling again," and the executor re-queues it.

This is the exact shape of a traditional **wait-queue + `wake_up()`**:

| Traditional | Futures |
|---|---|
| Add `current` to a wait-queue, set `TASK_INTERRUPTIBLE`, call `schedule()` | Stash `cx.waker()`, return `Poll::Pending` |
| `wake_up(&wq)` from an IRQ or another thread | `waker.wake()` |
| Scheduler eventually runs the woken thread | Executor eventually re-polls the woken future |

## `Pin`: why futures don't move

A self-referential state machine (one whose fields point into itself) must not be
moved in memory once it is being polled. `Pin<&mut Self>` is the type-level promise
"this will not move." You will see `Pin<Box<dyn Future>>` as the stored form of a
task. For this series you only need to know: *a running future has a stable
address*, which is what lets the rest of the system hold references to its slot.

## The executor's job

An executor (the reactor, in our case) is a loop:

```
loop:
    take a future that is ready to make progress
    build a Waker that re-queues this future when called
    poll(future, Context::from(waker))
    match result:
        Ready(_) -> the future is done; drop it
        Pending  -> leave it parked; it stashed the Waker, someone will wake it
```

That is the entire contract. Everything in [Part 1](01_reactor-as-scheduler.md) is
this loop dressed up with run-queues, priorities, time-slices, and multiple harts.

## Summary

- A future is **pull-driven**: it makes progress only when `poll`ed, and consumes
  nothing in between.
- `async fn` compiles to a **state machine struct**; its fields are the context
  saved across `.await` points. This replaces the kernel stack as the home of
  suspended state.
- `Poll::Pending` = "suspended"; `Poll::Ready(v)` = "done with `v`".
- A `Waker` is a re-poll request. `waker.wake()` is the futures spelling of
  `wake_up()`.
- An **executor** is a loop that polls ready futures and parks pending ones. The
  reactor is that loop.

Keep the mapping table from the [README](README.md) open. Next we meet the
executor itself.

---

**Anchors:** `core::future::Future`, `core::task::{Poll, Waker, Context}` (std/core
library, not in-tree). In-tree usage begins in `crates/tx-reactor/src/runtime.rs`.

**Next:** [Part 1 — The Reactor as a Scheduler](01_reactor-as-scheduler.md)
