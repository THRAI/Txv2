# 2026-06-02: AP observe and userspace scheduling dispatch status

## Executive summary

AP observation is now proven for rv64-qemu SMP live-drain captures.

Userspace work dispatch is still not enabled by default. The current OSComp
userspace scheduling policy pins every userspace thread to the submit hart, so
the scheduler's existing spread, steal, and rebalance machinery cannot move
those tasks. Enabling userspace dispatch is therefore a policy flip with a
large correctness blast radius, not merely a scheduler-tuning change.

The immediate result of this worktree is an observability prerequisite:

- rv64-qemu now has one exported observe ring per boot hart.
- AP entry initializes `tx-observe` before entering the AP reactor loop.
- A stable marker, `debug.observe.ap.init`, proves AP producer registration.
- A four-hart `pthread-minimal1` live-drain run captured records from harts
  0, 1, 2, and 3 with no loss or overwrite.
- A cfg-gated pthread-child dispatch experiment now exists:
  `tx_userspace_child_spread_smp1` and `tx_userspace_child_spread_smp4`.
  The default path remains single-hart pinned, while the experiment gives new
  pthread children an online-CPU affinity mask plus `spread_on_submit` without
  enabling post-entry migration, steal, or rebalance.
- The first SMP4 gated run exposed a separate first-userspace placement bug:
  when OpenSBI selected a nonzero boot hart, initial userspace followed that
  hart and faulted before pthread child creation. Initial userspace now pins to
  CPU0 when CPU0 is online, with a host regression test for current CPU3.
- After that fix, the SMP4 gated `pthread-minimal1` live run completes
  losslessly. The run is still CPU0-heavy, so it validates AP observe and the
  gated submission path without proving broad AP userspace execution yet.

## Worktree

Branch: `codex/ap-observe-init`

Path: `/Users/3y/.codex/worktrees/ap-observe-init/Tx`

Progress record: `docs/progress/worktrees/2026-06-02-ap-observe-init.json`

## AP observe implementation status

### Ring backing

`boards/tx-hal-riscv64-qemu-virt/src/lib.rs` now exposes the live-drain symbol
that the daemon already expects:

- `TX_OBSERVE_RINGS`
- `OBS_RING_HARTS = MAX_BOOT_CPUS`
- `OBS_RING_BYTES = 2 * 1024 * 1024`

This keeps the static observe footprint at 8 MiB total for the four-hart
rv64-qemu configuration, matching the live workflow default:

```sh
--hart-count 4 --ring-bytes 2097152
```

The previous private `OBS_RINGS` symbol was enough for kernel-side serial dump
paths but not for host live-drain, which resolves `TX_OBSERVE_RINGS` from the
kernel ELF.

### AP initialization

`crates/tx-kernel/src/init.rs` AP entry now calls:

```rust
tx_observe::init::<P>(cpu_id)
```

after substrate AP initialization and before `init_later_secondary`,
trap-vector install, online marking, and the secondary reactor loop.

It then emits:

```text
debug.observe.ap.init
```

with the AP CPU id as the counter value.

### Host names and workflow

`xtask/src/observe.rs` adds the stable name for `debug.observe.ap.init`. The
same file also now carries the missing `observe oscomp-live` wrapper and
`pass_optional_flag` helper that the clean branch dispatcher referenced but did
not define. Without those helpers, `cargo xtask progress validate` and the
normal observe workflow could not compile `xtask`.

## Observe evidence

Command:

```sh
cargo xtask observe oscomp-live \
  --name ap-observe-init-pthread-minimal1-20260602e \
  --test pthread-minimal1 \
  --timeout 900 \
  --source-data /Users/3y/Downloads/Tx/target/oscomp/testdata-full
```

Worktree-local prerequisites were initialized before the successful run:

- `git submodule update --init external/libc-bench`
- `tools/images/fetch-busybox.sh`

Artifact:

```text
target/oscomp/custom-run/ap-observe-init-pthread-minimal1-20260602e
```

Serial result:

```text
Platform HART Count         : 4
Domain0 HARTs               : 0*,1*,2*,3*
txkernel:qemu-riscv64-virt:smp:aps:online
txkernel:qemu-riscv64-virt:reactor:ap-loop:ok
txkernel:qemu-riscv64-virt:reactor:ap-runqueue:ok
txkernel:qemu-riscv64-virt:reactor:sched:stats:h0=3/2:h1=1/1:steal=0:rebalance=0
#### OS COMP TEST GROUP START libcbench-musl ####
b_pthread_createjoin_minimal1 (0)
  time: 53.009479000, virt: 0, res: 0, dirty: 0
#### OS COMP TEST GROUP END libcbench-musl ####
txkernel:qemu-riscv64-virt:userspace:exited:0
```

Live runtime summary:

| field | value |
| --- | ---: |
| `complete` | true |
| `hart_count` | 4 |
| `ring_bytes` | 2,097,152 |
| `symbol` | `TX_OBSERVE_RINGS` |
| `raw_records` | 1,321,459 |
| `lost_records` | 0 |
| `overwritten_records` | 0 |
| `repairs` | 0 |

Final ring cursor state:

| hart | producer | consumer | visible at shutdown |
| ---: | ---: | ---: | ---: |
| 0 | 1,321,368 | 1,321,368 | 0 |
| 1 | 34 | 34 | 0 |
| 2 | 29 | 29 | 0 |
| 3 | 28 | 28 | 0 |

`visible_records=0` at shutdown is expected for a completed live-drain run: the
host daemon advances each ring's consumer cursor as it writes raw records.

Rawrecords decode:

| hart | decoded records |
| ---: | ---: |
| 0 | 1,321,368 |
| 1 | 34 |
| 2 | 29 |
| 3 | 28 |

AP init marker:

| marker | counter id | hart | value | count |
| --- | ---: | ---: | ---: | ---: |
| `debug.observe.ap.init` | 443,844,819 | 1 | 1 | 1 |
| `debug.observe.ap.init` | 443,844,819 | 2 | 2 | 1 |
| `debug.observe.ap.init` | 443,844,819 | 3 | 3 | 1 |

The AP marker lives in the counter payload as `payload.counter_id`, not as a
top-level `name` column in the analyzer SQL views. The direct rawrecords decode
used:

```python
records = load_rawrecords(..., sort_records=False)
payload["counter_id"] == fnv1a32("debug.observe.ap.init")
```

Derived analyzer summary:

| table | rows |
| --- | ---: |
| `spans` | 31,858 |
| `counters` | 994,030 |
| `allocation_rows` | 66,770 |
| `sched_intervals` | 0 |
| `lock_rows` | 0 |

Top pthread-minimal1 spans in this run:

| span | n | total |
| --- | ---: | ---: |
| `sys_wait4` | 1 | 53.188409s |
| `sys_clone` | 2,501 | 6.987316s |
| `sys_futex` | 5,609 | 6.105369s |
| `sys_munmap` | 2,500 | 4.120368s |
| `sys_rt_sigprocmask` | 10,007 | 4.113060s |
| `drive.FutexWaitOp` | 2,264 | 3.443439s |
| `sys_mmap` | 2,500 | 3.171806s |

Interpretation: observe is now multi-hart, but the workload is still dominated
by hart 0 userspace execution in this specific run. AP activity is limited to
boot/reactor/scheduler-side records because userspace tasks remain pinned.

## Scheduling status

### What is implemented

The scheduler and reactor already have the main SMP mechanisms:

- per-hart queues and shared task metadata
- task placement and remote wake/IPI plumbing
- migration eligibility via `can_migrate`
- spread-on-submit for non-kernel tasks that explicitly request it
- work stealing across hart locals
- rebalance using the steal path

The active SMP design target is in `docs/Txv3/10_SCHED_SMP_v1.md`: secondary
harts should eventually pull user tasks, cross-hart wake must avoid lost wakeups,
and cpuset-compatible affinity should be respected.

### Why OSComp userspace work is still on one hart

The current userspace scheduling policy is explicit:

```rust
fn userspace_thread_sched_meta_for(cpu_id: CpuId) -> InitialSchedMeta {
    let affinity = Self::cpu_bit(cpu_id);
    InitialSchedMeta::fair()
        .with_affinity(affinity)
        .pinned()
        .userspace_thread()
}
```

That code path has a comment naming the reason: userspace trap/return state
still has hart-local architectural coupling.

The resulting policy chain is:

1. Userspace thread metadata gets a single-bit affinity mask for the submit CPU.
2. `.pinned()` sets `MigrationPolicy::Pinned`.
3. scheduler metadata computes `can_migrate = migration == Movable`.
4. default initial placement returns the first hart in the affinity mask because
   default userspace metadata does not set `spread_on_submit`.
5. steal eligibility requires `meta.can_migrate`.
6. rebalance uses the same steal path.

Pthread clone submission preserves this behavior. The clone path captures
`submit_cpu = current_cpu_id()` and submits children with:

```rust
userspace_thread_sched_meta_for(submit_cpu).preempted_on_submit()
```

So once the initial userspace thread is pinned, its child thread submissions
stay on the same hart.

### Current evidence for no userspace work stealing

The older full-pthread process-DS observe report in the main checkout found:

- decoded spans all on one hart
- process DS rows all on one hart
- `steal=0`
- `rebalance=0`

The new AP-observe run changes the observability conclusion, not the scheduling
policy conclusion. APs can emit observe records now, but userspace work still
does not spread because policy makes it ineligible.

## Gated child-spread experiment

### Code shape

The new experiment intentionally stops short of full userspace migration.

`crates/tx-kernel/build.rs` declares two custom cfg gates so Rust's check-cfg
lint accepts them:

- `tx_userspace_child_spread_smp1`
- `tx_userspace_child_spread_smp4`

`crates/tx-kernel/src/init.rs` keeps the default helper unchanged:

```rust
fn userspace_thread_sched_meta_for(cpu_id: CpuId) -> InitialSchedMeta {
    let affinity = Self::cpu_bit(cpu_id);
    InitialSchedMeta::fair()
        .with_affinity(affinity)
        .pinned()
        .userspace_thread()
}
```

It adds a separate child helper. With both experiment cfgs closed, that helper
delegates to the default pinned policy. With either experiment cfg open, it uses
`P::online_cpus().bits()` as the child affinity mask, falls back to the submit
CPU if the online mask is unexpectedly empty, and sets:

```rust
.pinned().spread_on_submit().userspace_thread()
```

`crates/tx-kernel/src/init/reactor_submit.rs` uses the child helper only for
pthread child submission:

```rust
userspace_child_thread_sched_meta_for(submit_cpu).preempted_on_submit()
```

The initial userspace task still uses `userspace_thread_sched_meta()`, but that
helper now chooses CPU0 when CPU0 is online and only falls back to the current
CPU if CPU0 is absent. This keeps the first userspace entry on the BSP-safe
path while limiting the experiment to pthread children.

### Scheduler semantics

`crates/tx-reactor/src/scheduler.rs` now lets `spread_on_submit` participate in
initial placement even when a task is pinned. This is narrow by construction:

- initial placement can spread among allowed harts when `spread_on_submit` is
  true;
- `can_migrate` remains false for pinned tasks;
- `try_steal` still requires `meta.can_migrate`;
- rebalance continues to use the steal path, so it cannot move these pinned
  children after entry.

The focused regression test is:

```text
pinned_spread_on_submit_uses_initial_spread_without_enabling_steal
```

It submits two pinned userspace tasks with a two-hart affinity mask and
`spread_on_submit`, verifies that initial placement splits them across harts,
and verifies that a later steal attempt still fails.

### SMP4 live result

The first SMP4 live experiment compiled and booted, but it did not reach the
pthread workload:

```sh
RUSTFLAGS="--cfg tx_userspace_child_spread_smp4" \
cargo xtask observe oscomp-live \
  --name ap-child-spread-smp4-pthread-minimal1-20260602a \
  --test pthread-minimal1 \
  --timeout 900 \
  --source-data /Users/3y/Downloads/Tx/target/oscomp/testdata-full
```

Artifact:

```text
target/oscomp/custom-run/ap-child-spread-smp4-pthread-minimal1-20260602a
```

The serial reached boot and first userspace submission:

```text
txkernel:qemu-riscv64-virt:bootstrap-exec:ok
txkernel:qemu-riscv64-virt:boot:ok
txkernel:qemu-riscv64-virt:userspace:submitted
txkernel:qemu-riscv64-virt:trap
reason=trap-action-terminate
scause=0x000000000000000c sepc=0x00000000006a1ee0 stval=0x00000000006a1ee0
```

`cargo xtask fault-decode --target rv64-qemu --serial ... --all --brief`
classified the trap as:

```text
trap #1: instruction page fault  user:0x6a1ee0  @ 0x00000000006a1ee0  (from S-mode)
```

The expanded decode shows `sstatus.spp=S`, `sepc=stval=0x6a1ee0`, and a return
address inside `tx_hal_riscv64_qemu_virt::pmap::pt_node::alloc_pt_node_from_bag`.

Live-drain finalized after manually terminating QEMU:

| field | value |
| --- | ---: |
| `complete` | true |
| `hart_count` | 4 |
| `raw_records` | 90 |
| `lost_records` | 0 |
| `overwritten_records` | 0 |
| `repairs` | 0 |
| decoded `spans` | 0 |
| decoded `counters` | 90 |

Ring producer counts were `h0=34`, `h1=28`, `h2=28`, `h3=0`; this is AP/boot
observe evidence only, not a successful AP userspace run.

Important interpretation: because the trap occurs immediately after initial
userspace submission, before `pthread-minimal1` starts and before pthread child
creation, this artifact does not prove that the child-spread call site itself is
faulting.

Root cause for this artifact was initial userspace placement, not pthread child
submission. `userspace_thread_sched_meta()` previously pinned to the current
boot CPU. QEMU/OpenSBI can select a nonzero boot hart, so the first userspace
entry could run through a non-BSP path before the child-spread experiment even
started.

The fix keeps initial userspace on CPU0 when CPU0 is online:

```rust
fn userspace_thread_sched_meta() -> InitialSchedMeta {
    let cpu0 = CpuId(0);
    let cpu = if P::online_cpus().contains(cpu0) {
        cpu0
    } else {
        P::current_cpu_id()
    };
    Self::userspace_thread_sched_meta_for(cpu)
}
```

The regression test is:

```text
initial_userspace_sched_meta_stays_on_cpu0_when_boot_hart_is_nonzero
```

The fixed SMP4 gated run is:

```sh
RUSTFLAGS="--cfg tx_userspace_child_spread_smp4" \
cargo xtask observe oscomp-live \
  --name ap-child-spread-smp4-pthread-minimal1-20260602c \
  --test pthread-minimal1 \
  --timeout 900 \
  --source-data /Users/3y/Downloads/Tx/target/oscomp/testdata-full
```

Artifact:

```text
target/oscomp/custom-run/ap-child-spread-smp4-pthread-minimal1-20260602c
```

Serial result:

```text
Boot HART ID                : 0
txkernel:qemu-riscv64-virt:reactor:sched:stats:h0=3/2:h1=1/1:steal=0:rebalance=0
txkernel:qemu-riscv64-virt:userspace:submitted
txkernel:qemu-riscv64-virt:bench:child_submit:submitted=2304
txkernel:qemu-riscv64-virt:bench:child_submit:direct=2304
txkernel:qemu-riscv64-virt:bench:child_submit:terminal_drained=2304
  time: 145.376583000, virt: 0, res: 0, dirty: 0
#### OS COMP TEST GROUP END libcbench-musl ####
txkernel:qemu-riscv64-virt:userspace:exited:0
```

Live runtime summary:

| field | value |
| --- | ---: |
| `complete` | true |
| `hart_count` | 4 |
| `ring_bytes` | 2,097,152 |
| `raw_records` | 1,402,925 |
| `lost_records` | 0 |
| `overwritten_records` | 0 |
| `repairs` | 0 |

Final ring cursor state:

| hart | producer | consumer | visible at shutdown |
| ---: | ---: | ---: | ---: |
| 0 | 1,402,871 | 1,402,871 | 0 |
| 1 | 25 | 25 | 0 |
| 2 | 19 | 19 | 0 |
| 3 | 10 | 10 | 0 |

Derived analyzer summary:

| table | rows |
| --- | ---: |
| `spans` | 34,083 |
| `counters` | 1,062,393 |
| `allocation_rows` | 66,770 |
| `sched_intervals` | 0 |
| `lock_rows` | 0 |

Top spans in the fixed SMP4 gated run:

| span | n | total |
| --- | ---: | ---: |
| `sys_wait4` | 1 | 145.915977s |
| `sys_futex` | 6,346 | 19.865295s |
| `sys_clone` | 2,501 | 14.736433s |
| `drive.FutexWaitOp` | 2,475 | 12.972667s |
| `yield.OnWaitSource` | 1,382 | 10.819522s |
| `sys_rt_sigprocmask` | 10,007 | 9.535844s |
| `sys_munmap` | 2,500 | 7.834853s |
| `sys_mmap` | 2,500 | 6.689279s |

Interpretation: the fixed run validates the original error is gone: SMP4 boots,
enters userspace, creates pthread children through the gated path, and exits
cleanly with a complete lossless observe capture. It does not yet prove broad
AP userspace execution. The ring distribution remains overwhelmingly hart 0,
with AP harts only producing low-volume AP/init/reactor records in this run.

### SMP1 control status

The SMP1 gated control was attempted with:

```sh
RUSTFLAGS="--cfg tx_userspace_child_spread_smp1" \
cargo xtask observe oscomp-live \
  --name ap-child-spread-smp1-pthread-minimal1-20260602a \
  --test pthread-minimal1 \
  --smp 1 \
  --timeout 900 \
  --source-data /Users/3y/Downloads/Tx/target/oscomp/testdata-full
```

It did not produce a kernel verdict because the host live-drain path failed with
`No space left on device (os error 28)`. Disk was later freed enough to run the
SMP4 control above, but the SMP1 control itself has not been rerun.

## Blast radius for enabling userspace work dispatch

### Policy gate

The smallest code flip is in `userspace_thread_sched_meta_for`: use a multi-hart
online/allowed mask, call `.movable()`, and likely call `.spread_on_submit()`.

That should be gated. Recommended shape:

- default: keep pinned userspace
- experimental boot parameter: enable movable userspace submit/spread
- optional cfg: compile out the experiment in strict baselines

Do not enable this by default until the userspace-slot, trap-runtime, and VM
residency audits below pass.

### Trap handoff and per-hart userspace slots

Highest correctness risk.

`PerHartSlotted` and the userspace trap path use hart-local current payload
slots. Pinned userspace makes that conservative. Movable userspace means a task
can enter userspace on hart A, trap and unwind, then later enter on hart B.

The audit has to prove:

- old hart userspace slots are cleared when a task migrates
- timer-preempt preservation does not leave a stale payload on the old hart
- `current_userspace_thread_identity` follows the active userspace payload
- no trap on hart A can resolve to a payload that has migrated to hart B

### RV64 resume context

High correctness risk.

The RV64 userspace entry path writes a per-hart `KernelResumeCtx` before
entering user mode. A trap-shell reschedule longjmp returns through the current
hart's resume context. Migration is only safe after that userspace round trip
fully unwinds to the Rust future; it must never resume a still-active
userspace-entry frame on a different hart.

### VM and ASID residency

Medium-high correctness and performance risk.

The HAL has ASID-residency-shaped support, but movable userspace will make the
same address space resident on multiple harts. That expands remote SFENCE
targets during `mmap`, `munmap`, `mprotect`, COW, and exit teardown. The pthread
minimal run already shows `mmap`/`munmap` as major costs, so dispatch should be
measured with VM lock/shootdown probes enabled before promotion.

### Wake/IPI volume

Medium risk.

Remote placement calls `send_reschedule_ipi` when the target hart differs from
the current hart. Once userspace work spreads, clone, futex wake, signal wake,
exit/wait, and mailbox paths can all become remote wake paths. The useful
measure is not just "did it run", but IPI count, wake latency, and whether
futex wake-to-pick latency regresses.

### Linux-visible ABI

Medium risk.

`sched_setaffinity`/`sched_getaffinity` already route through reactor affinity
seams, but they have not been validated under real movable userspace. `getcpu`
still reports CPU 0/node 0, which becomes wrong once user code truly runs on
multiple harts.

## Recommended next gate

Before enabling default userspace dispatch:

1. Add a boot-parameter gate for movable userspace scheduling.
2. Add observe counters for userspace placement, steal attempts, accepted
   steals, rebalance moves, remote wake/IPI, and userspace slot migration.
3. Run `pthread-minimal1` with the gate on and assert:
   - AP harts have userspace span records, not only AP init/reactor counters.
   - no trap/fault/panic lines appear.
   - `lost_records=0`, `overwritten_records=0`, `repairs=0`.
4. Run full `pthread` with VM lock/shootdown probes before considering
   promotion.
5. Fix or gate `getcpu` before exposing the behavior to Linux affinity tests.

## Verification

Passed:

```sh
cargo fmt --check
cargo test -p tx-reactor pinned_spread_on_submit_uses_initial_spread_without_enabling_steal -- --nocapture
cargo test -p tx-reactor --test scheduler -- --nocapture
cargo test -p tx-kernel initial_userspace_sched_meta_stays_on_cpu0_when_boot_hart_is_nonzero -- --nocapture
cargo test -p tx-kernel reactor_submission_seam_submits_child_thread_smoke -- --nocapture
cargo test -p tx-observe
cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
RUSTFLAGS="--cfg tx_userspace_child_spread_smp1" cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
RUSTFLAGS="--cfg tx_userspace_child_spread_smp4" cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo xtask progress validate
git diff --check
cargo xtask observe oscomp-live --name ap-observe-init-pthread-minimal1-20260602e --test pthread-minimal1 --timeout 900 --source-data /Users/3y/Downloads/Tx/target/oscomp/testdata-full
RUSTFLAGS="--cfg tx_userspace_child_spread_smp4" cargo xtask observe oscomp-live --name ap-child-spread-smp4-pthread-minimal1-20260602c --test pthread-minimal1 --timeout 900 --source-data /Users/3y/Downloads/Tx/target/oscomp/testdata-full
```

Historical failure used for diagnosis:

```sh
RUSTFLAGS="--cfg tx_userspace_child_spread_smp4" cargo xtask observe oscomp-live --name ap-child-spread-smp4-pthread-minimal1-20260602a --test pthread-minimal1 --timeout 900 --source-data /Users/3y/Downloads/Tx/target/oscomp/testdata-full
```

This produced an early instruction page fault after `userspace:submitted`, before
pthread child creation. It is fixed by pinning first userspace to CPU0 when
CPU0 is online.

Blocked:

```sh
RUSTFLAGS="--cfg tx_userspace_child_spread_smp1" cargo xtask observe oscomp-live --name ap-child-spread-smp1-pthread-minimal1-20260602a --test pthread-minimal1 --smp 1 --timeout 900 --source-data /Users/3y/Downloads/Tx/target/oscomp/testdata-full
```

This control did not reach a kernel verdict because host live-drain failed with
`No space left on device (os error 28)`.

Known non-regression warnings:

- `tx_vm_recipe_bplus` remains an existing `unexpected_cfgs` warning in
  `tx-subsystems` during rv64 checks.
