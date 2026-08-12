# Chapter 6 — The portable handoff shell and the real mainline (H2–H3)

Chapter 4 ended with `_start` jumping to the binary's `rust_entry`. Chapter 5
showed what happens *inside* `boot_handoff`. This chapter connects them: the thin
cross-platform shell `tx_hal::entry` (stage H2), and the generic kernel mainline
it hands off to (stage H3) — including the gap between the doc's idealized
`kernel_main` and the real `CoreInit::boot`.

## H2 — `tx_hal::entry::<P, K>`

`rust_entry` (in the binary) does exactly one thing:

```rust
#[no_mangle]
pub extern "C" fn rust_entry(cpu_id: usize, firmware_arg: usize) -> ! {
    tx_hal::entry::<ActivePlatform, Kernel>(cpu_id, firmware_arg)
}
```

`entry` lives in the *trait* crate, `crates/tx-hal/src/lib.rs:1450`:

```rust
pub fn entry<P, K>(cpu_id: usize, firmware_arg: usize) -> !
where
    P: TxPlatform,
    K: KernelMain<P>,
{
    P::install_minimal_trap_vector();
    let handoff = P::boot_handoff(cpu_id, firmware_arg);
    P::install_early_percpu(handoff.cpu_id);
    P::mark_cpu_online(handoff.cpu_id);
    K::kernel_main(handoff)
}
```

Two design points are worth dwelling on.

### The `K: KernelMain<P>` indirection

Why is `entry` generic over *two* type parameters — the platform `P` and a kernel
continuation `K`? Because `tx-hal` is *below* `tx-kernel` in the dependency
diamond (Chapter 1). The trait crate cannot name `tx_kernel::kernel_main`
directly; that would create a `tx-hal → tx-kernel` edge and collapse the diamond.

So the trait crate defines a trait (`crates/tx-hal/src/lib.rs:1434`):

```rust
pub trait KernelMain<P: TxPlatform> {
    fn kernel_main(handoff: BootHandoff) -> !;
}
```

and the *binary* supplies the type that implements it (the `Kernel` struct from
Chapter 1, whose `kernel_main` tail-calls `tx_kernel::kernel_main`). `entry`
calls `K::kernel_main(handoff)` and the monomorphizer wires it straight through.
The dependency arrow points only downward; the continuation is injected from the
top. This is a textbook use of a trait to invert a dependency without runtime
cost.

### The order of the four calls

The body is deliberately thin (the doc calls H2 "deliberately thin",
`txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H2-…-1`), but the order is load-bearing:

1. `install_minimal_trap_vector()` — *first*, so that any panic from this point on
   is catchable and printable. On rv64 this installs a direct-mode `stvec` that
   saves a frame and routes to the panic path.
2. `boot_handoff(cpu_id, firmware_arg)` — runs the entire `BootStaticBag` pipeline
   from Chapter 5 (DTB parse, identity-bridge drop, BootInfo/PlatformInfo publish)
   and returns the typed `BootHandoff`.
3. `install_early_percpu(handoff.cpu_id)` — makes per-CPU reads legal everywhere
   downstream. After this, `PercpuIf::current_cpu_id()` works (Chapter 14).
4. `mark_cpu_online(handoff.cpu_id)` — sets the BSP's bit in the online mask.

`BootHandoff` itself is the typed boot context (`crates/tx-hal/src/lib.rs:154`):

```rust
pub struct BootHandoff {
    pub cpu_id: CpuId,
    pub firmware_arg: BootArg,
    pub protocol: BootProtocol,   // RiscvSbi | RiscvDirect | LoongArchFirmware
}
```

The generic kernel receives this and never learns that `firmware_arg` was a DTB
or that `protocol` came from SBI. The doc's portable-boot contract
(`txdoc:HAL-THE-BOOT-SEQUENCE-PORTABLE-BOOT-CONTRACT-1`) is precisely this: raw
register meaning is platform-owned, `BootHandoff` is the typed surface, and "if a
new platform needs more decoded facts, add them to `BootInfo`/`PlatformInfo` or a
narrow trait — do not add board conditionals to `tx-kernel`."

> **Divergence — the dropped debug-assert.** The doc's `entry` sketch includes a
> `let _bi = P::boot_info(); debug_assert!(!_bi.memory_regions.is_empty());`
> right after `boot_handoff`. The shipped `entry` omits it — the equivalent
> validation moved into the `BootStaticBag` pipeline (the high-sentinel check and
> the DTB-parse fallback), so re-reading `boot_info()` here would be redundant.
> Minor, but it's a real difference between the doc text and the code.

## H3 — the doc's mainline vs the real one

The doc draws H3 (`txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H3-…-1`) as a clean linear
sequence: `init_early` → `substrate::init` → `init_later` →
`install_kernel_trap_vector` → `boot_secondary_cpus` → `reactor::init` →
`scheduler::init` → `vfs::init` → … → `exec::init_userspace`.

The shipped `kernel_main` (`crates/tx-kernel/src/lib.rs:67`) is one line that
delegates to a state machine:

```rust
pub fn kernel_main<P: TxPlatform + 'static>(handoff: BootHandoff) -> ! {
    init::CoreInit::<P>::boot(handoff)
}
```

`CoreInit::<P>::boot` (`crates/tx-kernel/src/init.rs:216`) is the real H3. Its
top-level shape:

```rust
pub fn boot(handoff: BootHandoff) -> ! {
    Self::init_early(handoff);
    Self::init_substrate_if_ready(handoff);
    if P::SUBSTRATE_BOOT_READY {
        Self::run_bootstrap_exec_for_init();   // seed init's saved_user_context
    }
    Self::boot_sentinel();                     // txkernel:<BOARD>:boot:ok
    if P::SUBSTRATE_BOOT_READY {
        Self::run_userspace_reactor_loop();    // enter userspace; returns when init exits
    }
    crate::zones::shutdown_with_zone_cleanup::<P>()
}
```

Two things to notice.

### `SUBSTRATE_BOOT_READY` gates the whole substrate-backed boot

This is the `PlatformConfig` constant we met in Chapter 3 (and which the doc never
mentions). A board that has only met the *smoke* contract leaves it `false`; the
mainline then skips substrate init, the userspace loop, and shuts down after the
sentinel — proving the firmware→kernel handoff works without requiring a frame
allocator. The RV64 QEMU board sets `SUBSTRATE_BOOT_READY = true`
(`lib.rs:272`), so it runs the full path. This is the doc's two-readiness-level
ladder (`txdoc:HAL-THE-BOOT-SEQUENCE-PORTABLE-BOOT-CONTRACT-1`) realized as a
single `const bool`.

### Everything the doc listed lives inside `init_substrate_if_ready`

The linear chain the doc imagined is real; it just lives inside one method gated
on readiness (`init.rs:259`, trimmed):

```rust
fn init_substrate_if_ready(handoff: BootHandoff) {
    if P::SUBSTRATE_BOOT_READY {
        init::<P>();                              // substrate: zones, frames, slab
        crate::zones::register_all()...;
        Self::init_later(handoff);                // P::init_later — heap-needing setup
        Self::install_kernel_trap_vector();       // replace the minimal H1 vector
        Self::init_boot_reactor();
        Self::boot_secondary_cpus();              // SBI HSM AP start (Chapter 14)
        Self::run_smp_shootdown_smoke();
        // … reactor / scheduler smokes …
        Self::init_process_subsystem();           // pid 1, init AddressSpace
        Self::prewarm_thread_runtime_caches();
        // ---- device + filesystem boot wiring ----
        Self::register_console_hardware();        // populate CONSOLE_TTY
        Self::install_irq_handlers();             // register + publish + unmask (Chapter 13)
        Self::init_block_devices();
        Self::mount_rootfs_from_boot_media();
        Self::mount_devfs_at_dev();
        // … procfs, sysfs, tmpfs, /dev/shm … …
        Self::bind_init_cwd_and_root();           // init gets cwd + fds 0/1/2
        Self::submit_net_runtime_tasks();
    }
}
```

The ordering comments in this method are some of the most carefully written in
the tree, because the dependencies are real: `install_kernel_trap_vector` must
follow `init_later`; `install_irq_handlers` must follow
`register_console_hardware` (the UART handler reads `CONSOLE_TTY`); devfs mount
must follow rootfs mount (devfs needs a `/dev` dentry to mount onto). This is the
doc's H4 ("downstream subsystem init") folded into H3, sequenced explicitly by
`kernel_main` exactly as the linkme discipline demands (Chapter 13): subsystem
ordering is encoded in the mainline, never in a link-time slice.

> **Divergence — user execution is live.** `HAL_v1 §11` (the TrapIf section)
> states plainly that "user execution is still not enabled: syscall dispatch, VM
> page-fault policy, … remain later slices." That sentence is now historical.
> `run_bootstrap_exec_for_init` execs the embedded `/init` fixture and seeds its
> `saved_user_context`; `run_userspace_reactor_loop` enters userspace through the
> production trap-return path and returns only when init zombifies. The trap path
> (Chapters 9–10) is fully wired. Treat the doc's "not enabled yet" language as a
> snapshot from before the userspace bring-up landed.

## The boot sentinel

The doc specifies a serial sentinel as the first executable proof a platform works
(`txdoc:HAL-THE-BOOT-SEQUENCE-SMOKE-SENTINEL-CONTRACT-1`): `txkernel:<P::BOARD>:boot:ok`,
emitted by generic kernel code so it proves the *selected platform axis* is in
use, not a hard-coded string. The implementation is exactly that. `boot_sentinel`
(`init.rs:2087`):

```rust
fn boot_sentinel() {
    Self::write_board_sentinel_prefix();
    tx_hal::console_write_str::<P>(":boot:ok\n");
}
```

and the prefix (`init/exec.rs:854`):

```rust
pub(super) fn write_board_sentinel_prefix() {
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
}
```

So the RV64 board prints `txkernel:qemu-riscv64-virt:boot:ok`, with the
`qemu-riscv64-virt` coming from `P::BOARD` (Chapter 3), not a literal. You can
watch for it with:

```sh
cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel
```

The same `write_board_sentinel_prefix` is reused all over `init.rs` to emit
sub-sentinels like `:process:init:ok` and `:thread-runtime:prewarm:payload=N`, so
the whole boot is observable as a stream of board-tagged checkpoints.

## `InitIf` — the two-stage platform callback

The mainline calls back into the platform at two defined points via `InitIf`
(`crates/tx-hal/src/lib.rs:311`):

```rust
pub trait InitIf {
    fn init_early(handoff: BootHandoff);   // after H2, before substrate::init
    fn init_later(handoff: BootHandoff);   // after substrate::init returns
    fn init_early_secondary(_cpu_id: CpuId) {}
    fn init_later_secondary(_cpu_id: CpuId) {}
}
```

The contract (doc §6): in `init_early` the heap is **not** up, the early console
**is**, per-CPU **is** installed, traps are minimal — so it may not allocate, take
traps, or wait. In `init_later` the heap **is** up and the direct map covers all
RAM — so it may `Box::new`, and map additional MMIO through `PmapIf`. On the RV64
QEMU board both are currently empty (`lib.rs:309`) — everything it needs is done in
`boot_handoff` and the pmap is brought up by substrate — but the seam exists so a
board with, say, a timer base to finalize or extra MMIO to map has a defined place
to do it.

## What you should take away

- `tx_hal::entry::<P, K>` is the thin H2 shell; `K: KernelMain<P>` injects the
  kernel continuation top-down so `tx-hal` never depends on `tx-kernel`.
- `entry` order is fixed: minimal trap vector → `boot_handoff` (the bag pipeline)
  → early per-CPU → online mark → kernel continuation.
- The real H3 is `CoreInit::<P>::boot`, gated on `P::SUBSTRATE_BOOT_READY`; the
  doc's linear `kernel_main` chain lives inside `init_substrate_if_ready` with
  carefully ordered device/FS wiring.
- The boot sentinel `txkernel:<P::BOARD>:boot:ok` is emitted from generic code
  using `P::BOARD`, proving the selected platform is in play.

This closes Part II. Part III dives into memory:
[Chapter 7 — Page tables behind a proof-object API](ch07-pmap-proof-objects.md).
</content>
