# Chapter 14 — Time, per-CPU state, and SMP mechanics

This chapter sweeps the remaining device-facing axes: `TimeIf` (the monotonic
clock and timer deadlines), `PercpuIf` (per-hart state), `SmpIf` (multi-hart
bring-up and IPIs), and the smaller `CacheIf`/`DmaIf`/`PowerIf`/`EntropyIf`. None
is individually large, but together they show how the HAL draws its boundary
against the higher-level subsystems (`SMP_v1`, the scheduler, the timer wheel)
that *consume* these primitives.

## `TimeIf` — monotonic ns and absolute deadlines

`crates/tx-hal/src/lib.rs:1206`:

```rust
pub trait TimeIf {
    fn read_ns() -> u64;                  // monotonic, non-decreasing, cheap
    fn set_deadline_ns(deadline: u64);    // absolute, same epoch as read_ns
    fn cancel_deadline();
    fn enable_timer_wakeups() {}
    fn frequency_hz() -> u64;
}
```

The contract is precise (doc §15): `read_ns` is monotonic, non-decreasing on the
current hart, and cheap enough for scheduler hot paths; `set_deadline_ns` takes an
*absolute* deadline in the same nanosecond epoch (not a relative duration), and a
past deadline must fire as soon as possible. The board (`lib.rs:587`) implements
`read_ns` over the unprivileged `rdtime` CSR scaled by the timebase, and
`set_deadline_ns` over the SBI timer call; `frequency_hz` comes from the
DTB-parsed `PlatformInfo.timebase_frequency_hz` (Chapter 5), with a fallback
constant.

The "absolute deadline" choice matters: it pushes the policy (when *should* the
next timer fire?) up to the timer wheel / reactor, and keeps the HAL purely
mechanical (program *this* instant). The HAL never decides scheduling quanta; it
arms whatever absolute time it's handed.

## `PercpuIf` — making "the current hart" addressable

`crates/tx-hal/src/lib.rs:1232`:

```rust
pub trait PercpuIf {
    fn current_cpu_id() -> CpuId { CpuId(0) }
    fn install_early_percpu(_cpu_id: CpuId) {}
    fn read_kernel_tls() -> u64 { 0 }
    fn write_kernel_tls(_value: u64) {}
    unsafe fn install_kernel_stack(_top: VirtAddr) {}
    fn pin_current_cpu() -> CpuPinGuard { CpuPinGuard::new(Self::current_cpu_id()) }
}
```

`install_early_percpu` is the call `tx_hal::entry` makes (Chapter 6) right after
`boot_handoff`, and it's what makes every later `current_cpu_id()` work. On the
board, per-hart state lives in `Rv64PerCpuArea` (`lib.rs:94`):

```rust
#[repr(C, align(64))]
pub struct Rv64PerCpuArea {
    cpu_id: usize,
    kernel_stack_top: AtomicUsize,
    irq_depth: AtomicUsize,
}
static RV64_PERCPU_AREAS: [Rv64PerCpuArea; MAX_BOOT_CPUS] = [ /* 0,1,2,3 */ ];
```

`tp` points at the current hart's area. Note the two distinct TLS notions:
`read_kernel_tls`/`write_kernel_tls` manage *kernel* per-hart storage, separate
from the *user* TLS register that the Chapter 9 trap vector swaps in and out. The
`irq_depth` field is what backs `IrqIf::in_irq_context()` from Chapter 13 — the
trap shell's `enter_irq_context()` bumps it, and the Chapter 13 EBR-guard
prohibition reads it. `CpuPinGuard` is a `!Send`/`!Sync` token (`lib.rs:67`)
proving "you are pinned to this hart for the guard's lifetime" — the type-system
encoding of "don't migrate between these two lines."

## `SmpIf` — bringing up the other harts

`SmpIf` (`crates/tx-hal/src/lib.rs:1306`) is the largest of these, because
multi-hart bring-up and inter-processor interrupts have real machinery. The board
implements it over SBI HSM (Hart State Management) and SBI IPI.

### Secondary bring-up

`boot_secondary_cpus(entry)` (`lib.rs:687`) is called from the boot sequence
(Chapter 6). Its shape is instructive:

```rust
fn boot_secondary_cpus(entry: SecondaryEntry) -> usize {
    let possible = Self::possible_cpus();
    IPI_ACKED_CPUS.store(0, Ordering::Release);
    pmap::install_secondary_identity_bridge();        // re-add the low bridge…
    for cpu in 0..MAX_BOOT_CPUS {
        if cpu == current || !possible.contains(cpu) { continue; }
        if start_secondary_hart(cpu, entry) { /* record started */ }
    }
    let online = wait_for_online_secondaries(started_mask);
    pmap::remove_secondary_identity_bridge();          // …and tear it back down
    online
}
```

The identity-bridge dance is the payoff of Chapter 5's typestate. The BSP dropped
its identity bridge during boot, but a *newly started* secondary hart begins
executing in low physical addresses (MMU off) and needs the identity mapping to
reach the trampoline — so `boot_secondary_cpus` temporarily reinstalls it, starts
the APs (each runs the Chapter 4 secondary trampoline into the same high
`rust_entry`), waits for them to mark themselves online, then removes the bridge
again. The bridge exists exactly as long as an AP might still need it.

`possible_cpus()` comes from the DTB CPU count (clamped to `MAX_BOOT_CPUS = 4`);
`online_cpus()` is an atomic bitmask updated by `mark_cpu_online`.

### IPIs and the membarrier handshake

`IpiKind` (`crates/tx-hal/src/lib.rs:1295`) enumerates the cross-hart messages:

```rust
pub enum IpiKind { Reschedule, TlbShootdown, Membarrier, Stop }
```

The board sends IPIs via SBI (`send_ipi`/`broadcast_ipi` → `send_sbi_ipi`), and
acknowledges them through an `IPI_ACKED_CPUS` atomic mask with
`ack_ipi`/`ipi_ack_cpus`/`clear_ipi_ack_cpus`/`wait_for_ipi_ack_cpus`. This
ack-tracking exists because some IPIs need a *completion* guarantee — the sender
must know every target has acted before proceeding.

`Membarrier` is the clearest example. Chapter 9's `on_ipi` handler does, for a
Membarrier IPI: `fence(SeqCst); ack_ipi(Membarrier)`. The `membarrier(2)` syscall
needs every hart to pass through a full memory fence so prior memory operations
are globally visible; the sender broadcasts the IPI and waits on
`wait_for_ipi_ack_cpus` until every target has fenced-and-acked. (`Membarrier` is
a divergence — the doc's `IpiKind` doesn't list it; it was added with the
`membarrier(2)` work. Minor; see [Appendix A](appendix-a-design-vs-code-ledger.md).)

`wait_for_interrupt_once()` is `wfi` on the board; `park_this_cpu()` enables
software interrupts and loops on `wfi`. The default `send_ipi`/`broadcast_ipi`
impls in the trait are *single-hart-safe stubs* (they assert the target is the
current CPU) so a uniprocessor or host platform links without real IPI hardware —
the board overrides them with the SBI versions.

### The HAL/`SMP_v1` boundary

The doc is careful (`txdoc:HAL-SMPIF-…-WHERE-THE-HAL-SMP-BOUNDARY-IS-1`) that
`SmpIf` provides only *mechanism*: start a hart, send/ack an IPI, read the online
mask, wait for an interrupt. It does **not** own *policy*: which CPU a task runs
on, how TLB shootdown batches are coalesced, when to send a reschedule IPI. That
all lives in `SMP_v1`/the scheduler above the HAL. You can see the seam in
Chapter 8: HAL `shootdown_mapping` does the local `sfence` + remote SBI RFENCE,
but the *decision* to shoot down (and the FrameMeta map-count bookkeeping that
makes it safe) is substrate's. The HAL kicks; the subsystem decides.

## The small axes

**`CacheIf`** (`lib.rs:629`): `fence_all`, `fence_i_local`/`fence_i_all`,
`flush_icache_range`, and dcache clean/invalidate. On the board, `fence_i_all`
issues an SBI remote `fence.i`. These exist so JIT/`mmap(PROT_EXEC)` code paths and
DMA drivers never `#[cfg]` on the architecture — they call `P::flush_icache_range`
and the board does the right instruction.

**`DmaIf`** (`crates/tx-hal/src/lib.rs:1277`): `phys_to_dma`/`dma_to_phys` and
`sync_for_device`/`sync_for_cpu`. QEMU `virt` has coherent DMA (`DMA_COHERENT =
true`), so the sync methods are no-ops and the address conversions are identity —
but the *seam exists* so a driver written against `DmaIf` works unchanged on a
non-coherent board where the syncs become real cache operations. This is the HAL's
"no-op now, real later, never `#[cfg]` in the driver" pattern.

**`PowerIf`** (`crates/tx-hal/src/lib.rs:1382`): just `system_off() -> !`. The
board calls SBI shutdown, then spins.

> **Divergence — `PowerIf` shrank.** The doc's `PowerIf`
> (`txdoc:HAL-POWERIF-…-1`) lists three methods: `system_off`, `reboot`,
> `cpu_off`. The shipped trait has only `system_off`. Reboot and per-CPU offline
> aren't needed by the current mainline, so they were dropped rather than left as
> unimplemented stubs. See [Appendix A](appendix-a-design-vs-code-ledger.md).

**`EntropyIf`** (`crates/tx-hal/src/lib.rs:1109`): `fill_random(out)`, "always
succeeds." The trait default is a deterministic xorshift counter (safe for
txKernel's current trust model: no ASLR, no untrusted input, musl SSP only); the
board overrides it to mix the `rdtime` CSR into that counter for a materially
better — though still not CSPRNG-grade — seed for `AT_RANDOM`. `EntropyIf` is one
of the two axes added to the supertrait after the doc was written (Chapter 2).

## What you should take away

- `TimeIf` is mechanical: monotonic `read_ns` and *absolute* `set_deadline_ns`;
  scheduling policy stays above the HAL.
- `PercpuIf` makes "the current hart" addressable (`tp` → `Rv64PerCpuArea`), with
  distinct kernel-TLS vs user-TLS notions and an `irq_depth` that backs
  `in_irq_context`.
- `SmpIf` provides bring-up + IPI *mechanism* (SBI HSM/IPI, ack-tracking for
  membarrier); *policy* lives in `SMP_v1`. The secondary identity bridge is
  reinstalled only for the duration of AP bring-up.
- `CacheIf`/`DmaIf` exist so drivers never `#[cfg]` on architecture even when the
  operation is a no-op on this board; `PowerIf` is trimmed to `system_off`;
  `EntropyIf` mixes `rdtime` into a counter.

This closes Part V. Part VI synthesizes:
[Chapter 15 — Porting to a second board](ch15-porting-second-board.md).
</content>
