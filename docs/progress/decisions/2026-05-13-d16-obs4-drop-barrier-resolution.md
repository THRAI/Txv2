# D16: OBS-4 Drop-Barrier Resolution

**Date.** 2026-05-13  
**Status.** Recommendation: Option C.  
**Scope.** `tx-observe`, `tx-substrate::wake::wait_source`, affected subsystem call sites.

---

## 1. Statement of the problem

OBS-4 introduced `WaitSource::notify_emit<P: PercpuIf>` so convergence points can emit `WaitSourceNotify` trace records carrying flow material. Of the 17 production call sites inventoried in the OBS-4 finisher report, 9 call bare `notify` instead of `notify_emit` because they cannot supply a `P: PercpuIf` type parameter. The three primary categories are: `Drop for OpenFile` and similar Drop impls (Rust forbids generic Drop), async kthread bodies (no caller-provided `P`), and deep ABI shims like `sys_io_submit` where `P` lives several frames up. These 9 sites are marked `TODO(OBS-4-followup)` in `pipe.rs`, `io_uring.rs`, `userfaultfd.rs`, `vfs/structure.rs`, `aio.rs`, `signalfd.rs`, `futex.rs`, `tty/execution/step_ingest.rs`, and `process/structure.rs`. Until they are resolved, a significant fraction of wake events are invisible to the trace daemon.

---

## 2. Why the current `P: PercpuIf` shape exists

`PercpuIf::current_cpu_id()` is the only operation OBS-4's `notify_emit` (and `tx_observe::current`) uses `P` for. On RISC-V it compiles down to a single `mv {out}, tp` instruction (`crates/tx-hal/src/lib.rs:1138`, board impl at `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:802`). The generic `P` parameter exists so the compiler can inline this single register read with zero indirection cost — no vtable, no atomic load. The cost of breaking that guarantee is one vtable dispatch or one `AtomicU64` load per `current()` call. Given that `current()` is called on the hot path of every emit (OBS-1: must be O(1) bounded), that cost is visible but small for a single-call path like `notify_emit`.

---

## 3. Existing patterns in the codebase

**`HartLocalArray<T, N>` in `tx-observe/src/hart_local.rs`.**  
`tx-observe` already uses its own `HartLocalOptionArray<HartEmitter, MAX_HARTS>` as `static EMITTERS` (`crates/tx-observe/src/lib.rs:223`). Getting a `HartEmitter` requires only a `usize` index — there is no `P` type parameter on `EMITTERS.get(idx)`. The only place `P` appears in the accessor chain is in `current::<P>()` at line 533–538, where it calls `<P as PercpuIf>::current_cpu_id().0` to obtain the index.

**Board `current_cpu_id` cost.**  
On RISC-V: one inline-asm `mv {out}, tp` with `options(nomem, nostack)` (`boards/tx-hal-riscv64-qemu-virt/src/lib.rs:807`). This is a single-cycle register read — effectively free. The TLS value written at boot (`install_early_percpu`) embeds the cpu-id in `tp` directly. On LoongArch it similarly reads a hardware TLS register (`boards/tx-hal-loongarch64-qemu-virt/src/platform_impls.rs:587`). Neither board loads from memory.

**Existing timestamp function pointer pattern.**  
`tx-observe/src/lib.rs:501` already uses a process-global `static TS_FN: AtomicU64` to store a bare function pointer to `TimeIf::read_ns`, installed at boot, then called without any generic at emit time. The generic `P` is only needed at `init` time to register the function; at call time `read_ts()` does one `Relaxed` atomic load and one indirect call.

**`init<P>` already stores the cpu-id accessor indirectly.**  
`observation_boot_hart::<P>(hart)` (spec §10, `crates/tx-observe/src/lib.rs:554`) takes `P: ObserverIf + TimeIf` and is the only place `P` is needed at boot. The `HartSlot` struct stores `hart_id: u8` at line 161, written from `hart.0` at line 637. Slots are indexed by that same `hart_id`. So `EMITTERS.get(idx)` already works with nothing more than a `usize` — it just needs the caller to supply one.

**No pre-existing per-hart static cpu-id accessor.**  
There is no existing `static CPU_ID_FN: AtomicU64` analogous to `TS_FN`. There is no `AtomicPtr<dyn PercpuAccessor>`. There is no `thread_local!` usage in kernel code (`thread_local!` appears only in `tx-subsystems/src/vm/tests.rs:22`, a std-using test).

---

## 4. Option A: per-hart singleton `AtomicPtr<dyn PercpuAccessor>`

Add a global `static CPU_ID_FN: AtomicU64` (matching the `TS_FN` pattern already in `tx-observe/src/lib.rs:501`) storing a bare function pointer `fn() -> usize`. Install it at boot alongside `TS_FN`. Change `current()` to drop the generic:

```rust
// tx-observe/src/lib.rs
static CPU_ID_FN: AtomicU64 = AtomicU64::new(0);

fn read_cpu_id() -> usize {
    let raw = CPU_ID_FN.load(Ordering::Relaxed);
    if raw == 0 { return 0; }
    let f: fn() -> usize = unsafe { core::mem::transmute(raw as usize) };
    f()
}

pub fn current() -> Option<&'static HartEmitter> {
    let idx = read_cpu_id();
    if idx >= MAX_HARTS { return None; }
    EMITTERS.get(idx).as_ref()
}

pub fn init_cpu_id_fn(f: fn() -> usize) {
    CPU_ID_FN.store(f as usize as u64, Ordering::Relaxed);
}
```

Cost: one `Relaxed` atomic load + one indirect call per `current()`. Install-order constraint: `init_cpu_id_fn` must be called before any emit fires; same discipline as `TS_FN` which already has this property.

This changes `current`'s public signature (removes `<P>`). That is the point.

---

## 5. Option B: `thread_local!`-like hart-context slot

`thread_local!` is unavailable in `no_std`. A per-hart static slot indexed by `current_cpu_id` would be another way to store a fat pointer to the platform vtable. But this is strictly more expensive than Option A: it requires reading `current_cpu_id` first (same cost), then indexing into a per-hart array to get the vtable pointer, then making an indirect call — two indirect loads instead of one. There is no benefit over Option A or Option C.

---

## 6. Option C: rely on the existing `HartLocalArray` plus a stored function pointer

This is Option A with the observation that the codebase **already has exactly this pattern** for timestamps. `TS_FN` at `crates/tx-observe/src/lib.rs:501` stores `fn() -> u64` as a bare function pointer in an `AtomicU64`, installed once at boot by `init_ts(P::read_ns)` (line 507). Reading it at emit time costs one `Relaxed` load and one non-virtual call.

The gap is that `current_cpu_id` was left generic while `read_ns` was moved to a function pointer. Adding an analogous `CPU_ID_FN: AtomicU64` — four lines of code mirroring what already exists — completes the pattern. The `HartLocalArray` and all slot-storage code remain unchanged. No new trait, no vtable, no `AtomicPtr<dyn Trait>`.

**Is it viable?**  
Yes. The board's `current_cpu_id` is a free function (`fn() -> CpuId`) that can be stored as a bare function pointer with no lifetime or trait-object overhead. `P` is only needed at init time to register it. After `init` runs, every emit path can use `read_cpu_id() -> usize` without a generic, exactly as `read_ts()` does today.

**There is no decision to make architecturally.** The framework already supports this; OBS-4 just needs to apply the same pattern to `current_cpu_id` that it already applied to `read_ns`.

---

## 7. Recommendation

**Option C.**

The `TS_FN` pattern in `tx-observe/src/lib.rs:501` is an exact precedent. One `static CPU_ID_FN: AtomicU64` and one `init_cpu_id_fn(f: fn() -> usize)` call in the existing `init<P>` body are the entire change in `tx-observe`. The public `current<P>()` signature drops its generic and becomes `current()`. No new traits, no vtable, no `AtomicPtr<dyn _>`.

Performance cost: one `Relaxed` atomic load + one non-virtual indirect call per `current()` invocation — identical to the existing `read_ts()` path. OBS-1 (O(1) bounded) is preserved.

Implementation cost: ~15 lines in `tx-observe/src/lib.rs`. Each of the 9 blocked call sites changes from `.notify()` to `.notify_emit()` with no other modifications. `drive.rs`, `wait_source.rs`, and all test stubs that currently pass a concrete `TestPlatform` type to `current::<TestPlatform>()` change to `current()`.

Test cost: the existing `obs4_convergence_point_emit.rs` test passes `TestPlatform2` to `notify_emit::<TestPlatform2>` only because `P` is a bound; after the change it calls `notify_emit()` directly. The mock platform still registers its cpu-id function via `init`. No new test harness is required.

---

## 8. Implementation plan

1. **`crates/tx-observe/src/lib.rs`**
   - Add `static CPU_ID_FN: AtomicU64 = AtomicU64::new(0)` (mirrors `TS_FN` at line 501).
   - Add `fn init_cpu_id(f: fn() -> usize)` (mirrors `init_ts` at line 506).
   - Add `fn read_cpu_id() -> usize` (mirrors `read_ts` at line 512).
   - Change `init<P: ObserverIf + TimeIf>` to `init<P: ObserverIf + TimeIf + PercpuIf>` and call `init_cpu_id(<P as PercpuIf>::current_cpu_id)` inside (alongside `init_ts`).
   - Change `pub fn current<P: PercpuIf>()` to `pub fn current()` — replace `<P as PercpuIf>::current_cpu_id().0` with `read_cpu_id()`.

2. **`crates/tx-substrate/src/wake/wait_source.rs`**
   - Remove `<P: PercpuIf>` from `notify_emit`. Change `tx_observe::current::<P>()` to `tx_observe::current()`.

3. **`crates/tx-scripts/src/drive.rs`**
   - Remove the `Plat: PercpuIf` bound on `drive<Plat, O, I>`. Change all four `tx_observe::current::<Plat>()` calls to `tx_observe::current()`. The `Plat` type parameter may be removable entirely if it has no remaining uses.

4. **9 blocked call sites** (in order of frequency): `pipe.rs` lines 312/333, `io_uring.rs` lines 368/387/393, `userfaultfd.rs` line 393, `vfs/structure.rs` lines 662/675, `aio.rs` lines 481/544, `signalfd.rs` line 256, `futex.rs` line 240, `tty/execution/step_ingest.rs` line 70, `process/structure.rs` line 630.
   - Replace `.notify(mask)` with `.notify_emit(mask)` and remove `TODO(OBS-4-followup)` comments.

5. **`crates/tx-substrate/tests/obs4_convergence_point_emit.rs`**
   - Change `simulate_last_writer_close::<TestPlatform2>` and `notify_emit::<TestPlatform2>` to non-generic calls.

6. **Run `cargo xtask progress validate`** and update `docs/progress/STATUS.md`.

---

## 9. Open questions and risks

**Install-order discipline.** If any hart calls `current()` before `init<P>` runs (e.g., a very early boot emit), `CPU_ID_FN` reads 0 and `current()` returns `None`. This is the same behavior as today when `P::observation_ring` returns `None`, and the same as the existing `TS_FN` path. No new hazard — but the boot sequencing comment in `observation_boot_hart` should be updated to mention both `init_ts` and `init_cpu_id`.

**`drive<Plat, O, I>` type parameter.**  
After `Plat: PercpuIf` is no longer needed for observation, `Plat` may have no remaining uses inside `drive`. If so, `drive` can drop the `Plat` type parameter entirely, removing a source of monomorphization pressure across ~178 `StepOp` impls. This is a collateral simplification, not a blocker.

**Remaining generic `P` uses in `tx-observe`.**  
`init<P: ObserverIf + TimeIf>` still needs `P: PercpuIf` to call `current_cpu_id` at init time. That bound is additive and does not change any public-facing API.

**This does not cover Drop-barrier sites that call into substrate code other than `notify_emit`.**  
If future OBS-8/9 work needs to emit from Drop contexts for non-`WaitSource` paths, the same `CPU_ID_FN` pattern applies directly.

**No risk to OBS-1 (O(1) bounded).** The emit path after this change is: one atomic load (`CPU_ID_FN`), one non-virtual function call (`current_cpu_id()`), one array index (`EMITTERS.get(idx)`). This is strictly equivalent to the timestamp path that has been in production since OBS-2.
