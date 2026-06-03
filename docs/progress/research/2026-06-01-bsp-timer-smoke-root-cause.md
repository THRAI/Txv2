# 2026-06-01: BSP timer smoke root cause

## Symptom

The manual shared-RAM OSComp observe path intermittently panicked before
userspace:

```text
assertion `left == right` failed: BSP timer smoke deadline
  left: None
 right: Some(...)
```

The panic was at `crates/tx-kernel/src/init.rs:1808`, immediately after
`reactor:runtime-loop:ok` and before `reactor:timer-idle:ok`.

## Root cause

`run_bsp_reactor_timer_idle_smoke()` computed an absolute timeout deadline
before submitting and polling the timer task:

```rust
let deadline_ns = P::read_ns().saturating_add(5_000_000);
```

The smoke then asserted that the first reactor step must report that same
deadline as `next_deadline_ns`.

That assumption is not stable under slow or preempted QEMU. If the BSP does
not first-poll the submitted task within the 5 ms smoke delta, the
`DeadlineFuture` sees `state.now_ns >= deadline_ns` on its first poll, returns
`TimedOut` immediately, drops the timer, and correctly leaves
`next_deadline_ns=None`. The assertion was therefore racing the time between
"deadline chosen" and "future first polled"; the timer queue was not losing a
registered waiter.

## Fix

The smoke now computes the timeout deadline inside the timer task's first poll,
stores it in `BSP_REACTOR_TIMER_DEADLINE_NS`, and then asserts that the first
reactor step armed that published deadline. This preserves the intended smoke
coverage:

- the task must be first-polled;
- the wait future must register a timeout;
- the reactor step must report the timeout as the next deadline;
- the later timer wake still has to complete the task and print
  `reactor:timer-idle:ok`.

## Evidence

Fixed shared-RAM run artifact:

```text
target/oscomp/custom-run/timer-smoke-rootcause-fixed-20260601-204035/serial-file.txt
```

The corrected run used the same class of manual shared-memory OSComp command
with `-smp 4`, `-accel tcg,thread=multi`, 1 GiB shared guest RAM, and:

```text
tx.oscomp.observe=1 tx.oscomp.observe_threshold=100000000 tx.oscomp.groups=libcbench-musl
```

Serial evidence:

```text
txkernel:qemu-riscv64-virt:reactor:runtime-loop:ok
txkernel:qemu-riscv64-virt:reactor:timer-idle:ok
txkernel:qemu-riscv64-virt:boot:ok
txkernel:qemu-riscv64-virt:userspace:submitted
#### OS COMP TEST GROUP START libcbench-musl ####
b_pthread_createjoin_serial1 (0)
b_pthread_createjoin_serial2 (0)
b_pthread_create_serial1 (0)
b_pthread_uselesslock (0)
b_pthread_createjoin_minimal1 (0)
```

`cargo xtask fault-decode --target rv64-qemu --serial
target/oscomp/custom-run/timer-smoke-rootcause-fixed-20260601-204035/serial-file.txt
--all --brief` found no `scause/sepc/stval` trap lines.

## Verification

- `cargo fmt --check`
- `RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_process" cargo check
  -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf -q`
- `cargo test -p tx-reactor --test timer_idle -- --nocapture`
- `RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_process" cargo
  xtask build --target rv64-qemu`
- Manual shared-RAM QEMU run above reached userspace and pthread benchmarks.

## Follow-up

The old stdout pipeline form for manual QEMU capture produced zero serial bytes
after the stale process was killed. The file-backed serial form
(`-display none -monitor none -serial file:<path>`) matched the xtask runner and
gave deterministic evidence. Prefer file-backed serial logs for future
root-cause reproductions.
