# Observation Subsystem — v1

<!-- txdoc:TXV3-OBSERVATION-V1 -->

**Status.** v1 (Txv3, 2026-05).
**Purpose.** Specify the kernel-side observation subsystem: where trace events are emitted, what they record, what invariants the trace path holds, what the host-side daemon expects to receive, and how the subsystem composes with the v3 framework (`StepOp`, `YieldShape`, `WaitSource`, `OnBehalfOf`, `RawTrace`). Closes the forward references in [`SCHEDULER_v0.md txdoc:SCHED-7-4-OBSERVATION-SUBSYSTEM-FUTURE`](../design/02_execution/SCHEDULER_v0.md), [`STEP_MODEL_v1.md txdoc:STEP-11`](../design/02_execution/STEP_MODEL_v1.md), and [`THREAD_RUNTIME_v1.md`](../design/02_execution/THREAD_RUNTIME_v1.md)'s "observation subsystem (future)" note.
**Audience.** Subsystem authors adding tracepoints; reviewers evaluating instrumentation PRs; subagents implementing the observation crates.
**Companion documents.**
- [`08_OBSERVATION_SERIALIZATION_v0.md`](08_OBSERVATION_SERIALIZATION_v0.md) — txtrace-v0 wire format (record schemas, ring layout, ABI).
- [`08_OBSERVATION_HOST_v0.md`](08_OBSERVATION_HOST_v0.md) — host daemon reconstruction spec.
- [`03_STEP_MODEL_v2.md`](03_STEP_MODEL_v2.md) — defines `StepOp`, `StepOutcome`, `YieldShape`.
- [`Reactor_concept_v5_RefactorSpec v4.md`](../design/02_execution/REACTOR_v0.md) — defines `WaitGeneration`, `TaskMailbox`, `WaitSource`.
- [`BUS_v1.md`](../design/01_substrate/BUS_v1.md) — defines `RawTrace<P>`, the substrate publication primitive observation extends.

---

## 1. Cleavage from existing primitives

<!-- txdoc:OBS-V1-CLEAVAGE-1 -->

This subsystem is *new*; it does not replace any existing primitive. Three adjacent surfaces, and how observation relates to each:

- **`RawTrace<P>` ([`BUS_v1` txdoc:BUS-THE-THREE-PRIMITIVES-1](../design/01_substrate/BUS_v1.md))** is a passive substrate publication point, nop-patched when no subscriber exists. v1 documents say RawTrace is "intentionally no-op until trace runtime/subscribers defined." This subsystem is RawTrace's first concrete subscriber for phase-level events (L4+). The substrate-internal convergence points (drive loop, `wake::notify`, `zone::sign`) emit observation events directly through this subsystem's emit API, not through RawTrace — RawTrace remains the per-subsystem hook for subsystem-internal tracepoints.
- **`ConsoleIf` ([`HAL_v1` txdoc:HAL-CONSOLEIF-1](../design/01_substrate/HAL_v1.md))** is the panic-time / `printk!`-time byte sink. Observation is a *separate channel*. ConsoleIf continues to serve early-boot, panic, and unstructured output; the observation transport is richer (per-hart rings, binary records, ivshmem on QEMU) but only available after the HAL's `ObserverIf` has handed back a `RingDescriptor`. ConsoleIf is the universal fallback.
- **VM stats cache ([`VM_v1_2` txdoc:VM-2-ADDRESSSPACE-STATS-CACHE](../design/03_memory-vm/VM_v1_2.md))** holds sampled state (rss, vm_size) for observability. Observation events *reference* those stats by reading them at emit time; they do not duplicate them in trace records. The cache is the truth source, the trace is the timeline.

OBS-V1-CLEAVAGE: observation is a passive timeline reconstruction layer over events the v3 architecture already factors into convergence points. It is not a logging system, a metrics system, a tracing-as-truth system, or a substitute for ptrace.

## 2. Architecture

<!-- txdoc:OBS-V1-ARCH-1 -->

### 2.1 Two-stage decoupling

The kernel does **not** emit Perfetto protobuf directly. The kernel writes fixed-size binary records (`txtrace-v0`, [companion spec](08_OBSERVATION_SERIALIZATION_v0.md)) into per-hart SPSC rings. A host-side daemon consumes the rings and reconstructs Perfetto's `TrackEvent` stream.

```text
kernel (any hart)
  -> tx_observe::emit_*  (per-hart static emitter)
  -> txtrace-v0 record (Pod-Plain, fixed-size, native-endian)
  -> per-hart SPSC ring (HAL-provided backing)
  -> [ivshmem | reserved DRAM | host-mapped buffer]
host
  -> tx-trace-daemon (mmap / file-replay / snapshot)
  -> txtrace-v0 decoder + span/flow reconstruction
  -> Perfetto .pftrace protobuf
  -> Perfetto UI
```

Rationale: no protobuf encoder in kernel; no dynamic allocation on the hot path; no string formatting in kernel; usable from early boot and panic contexts (subject to HAL availability); Perfetto wire-format changes are isolated to the host daemon, not kernel ABI.

### 2.2 Layering

<!-- txdoc:OBS-V1-ARCH-LAYERING-1 -->

| Layer | Crate | Role |
|---|---|---|
| HAL contract | `tx-hal::observer` | `ObserverIf` trait; returns per-hart `RingDescriptor` or `None`. |
| Per-board impl | `boards/tx-hal-*/src/observer.rs` | ivshmem on rv64-qemu; host-mapped on m1dock-mock; `None` on la64. |
| Wire-format types | `tx-observe-types` | `#[repr(C)]` POD structs shared kernel ⇄ host. `tx_hal::Pod`-marked. Host enables `Debug + serde` derives via `host` feature. |
| Kernel emitter | `tx-observe` | Per-hart `HartLocal<HartEmitter>`, ring write path, `emit_*` API, compile-time level gates. |
| Convergence-site emits | `tx-scripts::drive`, `tx-substrate::wake`, `tx-shims::adapter` | One-line `tx_observe::emit_*` calls at the points the v3 architecture already factors. |
| Host daemon | `tools/tx-trace-daemon` (separate cargo project) | mmap/file replay → Perfetto protobuf. |

OBS-V1-LAYERING: every layer except the convergence sites can be added to main as pure additions with zero behavior change. Convergence sites land behind compile-time level gates that default to off.

### 2.3 Convergence-point doctrine

<!-- txdoc:OBS-V1-ARCH-CONVERGENCE-1 -->

Trace events are emitted at convergence points the v3 architecture already factored:

| Convergence point | Code home | Levels emitted |
|---|---|---|
| Syscall U/K boundary | `tx-shims::adapter::traced_syscall!` | L0 Boundary |
| `drive<O: StepOp>` loop entry/exit | [`tx-scripts::drive`](../../crates/tx-scripts/src/drive.rs) | L2 Drive |
| `StepOp::step()` invocation (around the call site in `drive`) | `tx-scripts::drive` | L4 Step (as SpanBegin/SpanEnd) |
| `Yield { shape }` classification in `drive` | `tx-scripts::drive` | L3 Yield, opens flow |
| `wait_active` resume classification | `tx-scripts::drive` (or sibling) | L3 Yield close, closes flow |
| `WaitSource::notify` producer | `tx-substrate::wake` | L3 Yield (producer-side flow record) |
| `DelegateToken::reply` / `mark_*` | `tx-substrate::step::agent` | L3 Yield (OnAgent round-trip) |
| `zone::sign` / `index::commit` | `tx-substrate::zone`, `tx-substrate::index` | L6 Mutation |
| Five-stage phase boundaries inside `step()` bodies | per-subsystem `step_*.rs` via `RawTrace<P>` | L5 Phase |

OBS-V1-CONVERGENCE: instrumentation lives at convergence points only. Specifically: **adapter modules (`*/adapter.rs`) do not emit observation events**. Adapter verbs call substrate verbs; substrate verbs emit. This keeps the boundary report's "inside adapter" accounting aligned with trace coverage, and prevents observation from becoming a crosscutting concern at every subsystem boundary.

OBS-V1-NO-STEP-BODY: `StepOp::step()` bodies do not emit observation events directly. The driver wraps step calls with SpanBegin/SpanEnd. Phase-level events (L5) inside step bodies use `RawTrace<P>` from BUS_v1 and are nop-patched when no subscriber exists.

## 3. The level catalog

<!-- txdoc:OBS-V1-LEVELS-1 -->

Seven levels, each with a compile-time gate and a runtime `level` tag on every record.

| Level | Value | Compile gate | Runtime meaning | MVP |
|---|---|---|---|---|
| `Boundary` | 0 | `level_syscall` | User/kernel transition (syscall enter/exit, trap entry) | ✓ |
| `Script` | 1 | `level_drive` | Multi-drive syscall envelope (execve, ptrace, SQPOLL sub-script) | ✗ (deferred) |
| `Drive` | 2 | `level_drive` | One `drive<O>` invocation (begin/end) | ✓ |
| `Yield` | 3 | `level_yield` | ActiveWait interval between `Yield` and resume | ✓ |
| `Step` | 4 | `level_step` | One `StepOp::step()` invocation (SpanBegin/SpanEnd) | ✓ |
| `Phase` | 5 | `level_phase` | observe / upgrade / reserve / commit / publish boundary | ✗ (deferred) |
| `Mutation` | 6 | `level_mutation` | Substrate commit instant (`zone::sign`, `index::commit`) | ✗ (deferred) |

OBS-V1-LEVELS-ORTHO: compile-time gates and the runtime `level` tag are orthogonal. Compile-time gates exclude emit sites entirely (cost-when-off = zero). The runtime `level` field on each record exists for host-side filtering of compiled-in events.

OBS-V1-LEVELS-MVP-PROGRESSIVE: MVP includes L0/L2/L3/L4. L1 (Script) is deferred because most syscalls are single-drive and a nested-drive shape requires the multi-drive wrapper at the shim; this can land later without retrofit. L5/L6 wait on `RawTrace<P>` subscriber wiring and substrate-side hook placement; deferring keeps the convergence surface small for the first ship.

## 4. The HAL contract — `ObserverIf`

<!-- txdoc:OBS-V1-HAL-1 -->

Observation backing is platform-specific. The HAL provides the slab; substrate builds the ring abstraction over it.

```rust
// crates/tx-hal/src/observer.rs

/// Per-hart trace ring descriptor handed to substrate at boot.
///
/// `base` and `size` describe a contiguous region the kernel may write
/// into using release-store semantics. `doorbell`, when present, is a
/// device-specific notification register (ivshmem MSI base, etc.).
///
/// Lifetime: the region must remain valid for the kernel's runtime.
/// Boards backing the region with memory-backed files for crash recovery
/// keep the file mapped for the kernel's lifetime.
#[derive(Copy, Clone)]
pub struct RingDescriptor {
    pub base: core::ptr::NonNull<u8>,
    pub size: usize,                   // bytes; must be power of two
    pub doorbell: Option<core::ptr::NonNull<u32>>,
}

// SAFETY: RingDescriptor is sent across thread boundaries during boot
// init; the underlying region is shared but writes are coordinated by
// the per-hart SPSC discipline in tx-observe.
unsafe impl Send for RingDescriptor {}
unsafe impl Sync for RingDescriptor {}

pub trait ObserverIf {
    /// Called once during per-hart init. Returns this hart's ring slab
    /// or `None` if the board has no transport. `None` ⇒ kernel-side
    /// emit becomes a runtime no-op even when compile-time gates are on.
    fn observation_ring(hart: CpuId) -> Option<RingDescriptor> {
        None
    }

    /// Optional flush hint after a batch of emits. Default no-op.
    /// ivshmem: nothing (host polls). JTAG-TPIU: flush. Hosted-file: fsync.
    fn observation_flush(_hart: CpuId) {}

    /// True if the platform's trace clock is shared across harts
    /// (so cross-hart ordering is trustworthy without calibration).
    /// QEMU `time` CSR → true. Real RV64 silicon with per-hart timers → false.
    fn clock_shared() -> bool {
        false
    }
}
```

OBS-V1-HAL-SUPERTRAIT: `ObserverIf` is added to the `TxPlatform` aggregate ([`tx-hal/src/lib.rs` `pub trait TxPlatform`](../../crates/tx-hal/src/lib.rs)). All existing boards inherit the default-`None` impl; no board needs source changes for OBS-0 to land.

OBS-V1-HAL-RUNTIME-GATE: the `Option<RingDescriptor>` return value is the runtime gate. A kernel compiled with `level_step` enabled but running on a board whose `ObserverIf` returns `None` pays only the dead-code cost of the emit sites (essentially zero with `-O`).

## 5. Crate layout

<!-- txdoc:OBS-V1-CRATES-1 -->

Two new kernel-side crates, one new host-side tool, one new HAL file, one new file per board.

```text
crates/
├── tx-hal/src/
│   └── observer.rs              # NEW — ObserverIf trait + RingDescriptor (~80 LoC)
│
├── tx-observe-types/            # NEW — shared wire-format types (no_std)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs               # re-exports + Pod marker (unsafe impl tx_hal::Pod)
│       ├── header.rs            # TxTraceHeader, TxTraceHartRing
│       ├── record.rs            # TxTraceRecord, TxTraceKind, TxTraceLevel
│       └── payload.rs           # Payload* structs, TxPayloadTag, TxValueKind
│
├── tx-observe/                  # NEW — kernel emitter (no_std)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs               # public emit_* API + level features
│       ├── ring.rs              # SPSC ring over RingDescriptor
│       ├── emitter.rs           # HartEmitter; per-hart state on HartLocal
│       ├── intern.rs            # const string id helpers (TypeId-derived)
│       └── boot.rs              # observation_boot_hart(); called from kernel init
│
└── (existing crates instrumented at convergence sites:
   tx-shims, tx-scripts, tx-substrate)

boards/
├── tx-hal-riscv64-qemu-virt/src/observer.rs    # NEW — ivshmem BAR (~150 LoC)
├── tx-hal-riscv64-m1dock-mock/src/observer.rs  # NEW — None or hosted buffer
└── tx-hal-loongarch64-qemu-virt/src/observer.rs # NEW — None

tools/
└── tx-trace-daemon/             # NEW — host cargo project (not in kernel workspace)
    ├── Cargo.toml
    └── src/
        ├── main.rs
        ├── transport/           # ivshmem mmap, file replay, snapshot
        ├── decode/              # txtrace-v0 binary decoder
        ├── reconstruct/         # spans, flows, repair markers
        ├── perfetto/            # TrackDescriptor + TrackEvent emission
        └── output/              # .pftrace + debug JSON
```

OBS-V1-DEP-GRAPH:

```text
tx-observe-types  (no deps; defines Pod-marked POD structs)
       ↑
       ├── tx-hal  (defines Pod, ObserverIf; depends only on core types)
       │   ↑
       │   tx-observe  (depends on tx-hal for Pod/ObserverIf/PercpuIf/HartLocal/TimeIf
       │              + tx-observe-types for record shapes)
       │   ↑
       │   ├── tx-substrate  (calls tx_observe::emit_* at substrate convergence points)
       │   ├── tx-scripts    (calls tx_observe::emit_* in drive)
       │   └── tx-shims      (calls tx_observe::emit_* via traced_syscall! macro)
       │
       └── tx-trace-daemon  (host crate; depends on tx-observe-types with `host` feature)
```

OBS-V1-NO-CIRCULAR: `tx-observe-types` depends only on `tx-hal::Pod` (the existing trait at [`tx-hal/src/lib.rs` line 814](../../crates/tx-hal/src/lib.rs#L814)). Host daemon depends on `tx-observe-types` only — no kernel crates leak to host.

## 6. The hook surface — instrumentation map

<!-- txdoc:OBS-V1-HOOKS-1 -->

Concrete sites, what each emits, and which level.

| Site | File | Function | Records emitted | Level |
|---|---|---|---|---|
| Syscall entry | `tx-shims/src/adapter.rs` | `traced_syscall!` macro pre-arm | `SpanBegin(sys_<name>)` + `PayloadSyscallEnter` + ArgCont per arg | L0 |
| Syscall exit | `tx-shims/src/adapter.rs` | `traced_syscall!` macro post-return | `SpanEnd(sys_<name>)` + `PayloadSyscallExit` | L0 |
| Drive begin | `tx-scripts/src/drive.rs` | `drive<O>` entry | `SpanBegin(op_type)` + `PayloadDriveBegin` | L2 |
| Step begin | `tx-scripts/src/drive.rs` | before `op.step(ctx)` | `SpanBegin(step.iteration)` | L4 |
| Step outcome | `tx-scripts/src/drive.rs` | after `op.step(ctx)` returns | `SpanEnd(step.iteration)` + `PayloadStepOutcome` | L4 |
| Yield begin | `tx-scripts/src/drive.rs` | in `Yield { shape, progress }` arm of `drive` | `SpanBegin(yield.<shape>)` + `PayloadYieldBegin` | L3 |
| Resume | `tx-scripts/src/drive.rs` | after `wait_active` returns | `Instant(resume)` + `PayloadResume`; `SpanEnd(yield.<shape>)` | L3 |
| Drive end | `tx-scripts/src/drive.rs` | `drive<O>` exit (Done or Err) | `SpanEnd(op_type)` + result payload | L2 |
| WaitSource notify | `tx-substrate/src/wake/...` | `WaitSource::notify(mask)` | `Instant(wake.notify)` + flow_id | L3 (producer half) |
| DelegateToken reply | `tx-substrate/src/step/agent.rs` | `DelegateToken::reply` | `Instant(agent.reply)` + flow_id | L3 (OnAgent) |
| DelegateToken state | `tx-substrate/src/step/agent.rs` | `mark_agent_died` / `mark_timed_out` | `Instant(agent.<state>)` | L3 |
| Zone sign | `tx-substrate/src/zone/...` | `zone::sign(value)` | `Instant(mutation.zone_sign)` + object_id | L6 |
| Index commit | `tx-substrate/src/index.rs` | `index::commit(...)` | `Instant(mutation.index_commit)` | L6 |
| Phase boundaries | per-subsystem | via `RawTrace<P>::emit` | `Instant(phase.<name>)` | L5 |

OBS-V1-HOOK-SCOPE: every site in this table is in `tx-scripts`, `tx-shims`, `tx-substrate`, or a per-subsystem file. **No site is in an adapter module.** This is enforceable by `cargo xtask boundary-report` — extending the boundary lint to forbid `tx_observe::emit_*` calls inside `#[platform_adapter]`-marked modules is recommended (one PR after OBS-3 lands).

## 7. The discipline — invariants

<!-- txdoc:OBS-V1-INVARIANTS-1 -->

Numbered for cite-stability. Each is a compile-time, lint-time, or runtime-checkable constraint.

**OBS-1. Non-blocking.** The trace emit path never blocks kernel execution. Ring overflow drops records and increments the per-hart `lost` counter. Verified by inspection: emit code does not call `await`, does not lock anything except a per-hart-local SPSC head store, and does not enter any code path that can sleep.

**OBS-2. No user-pointer dereference.** Trace emit never dereferences a `UserPtr<T>`. Raw register-shaped syscall args are emitted as opaque `u64`. Decoded user data (paths, comms) is emitted *only* by the shim *after* the normal syscall copy-in path validates and copies.

**OBS-3. No allocation on hot path.** Trace emit is allocation-free. Record bodies are `#[repr(C)]` Pod-marked POD structs written into the ring slot by direct field stores. The per-hart `HartEmitter` is initialised once at boot and never reallocated.

**OBS-4. No string formatting on hot path.** Strings are emitted as `EventNameId` (u32) numeric identifiers. The host daemon resolves names from a build-emitted `names.json` keyed by `boot_id`. `format_args!`, `Debug`, or `Display` impls on traced objects must not be invoked from emit code.

**OBS-5. Observations are not semantic state.** Trace records must not affect syscall results, scheduling decisions, wakeup ordering, wait registration, or any subsystem state. A kernel with `level_*` features off and a kernel with all levels on must produce identical observable behavior modulo trace emission.

**OBS-6. Ring overflow drops records.** Producer rule: if `producer - consumer >= slot_count`, increment `lost`, do not write the record, return immediately. No spin, no fallback channel, no escalation.

**OBS-7. Zone-backed object ids include generation.** Trace records identifying zone objects emit `(kind, generation, slot)` packed as `u64` ([§12](#12-object-identifiers)). Slot-reuse after EBR retire cannot confuse trace-side identity.

**OBS-8. Same-hart ordering is `(hart, seq)`; cross-hart is timestamp-derived.** Same-hart total order is the per-hart ring sequence number. Cross-hart order is reconstructed by `ts`. When `ObserverIf::clock_shared()` is false, cross-hart timestamps are marked approximate by the host daemon.

**OBS-9. The daemon, not the kernel, owns Perfetto encoding.** Kernel emits txtrace-v0 records ([companion spec](08_OBSERVATION_SERIALIZATION_v0.md)). The daemon transcodes to Perfetto protobuf. Kernel ABI is txtrace-v0; Perfetto wire-format is a daemon-side concern.

**OBS-10. Record and payload struct layouts are frozen per version.** Field reordering, addition, or removal in any `#[repr(C)]` wire-format struct requires a version bump (`TxTraceHeader.version` or `TxTraceRecord.version`). Audit pre-merge with `cargo expand` to verify byte layout.

**OBS-11. No reentry.** The emit path must not call back into traced substrate code. Concretely: `tx_observe::emit_*` does not call `zone::sign`, `wake::notify`, `epoch::guard`, or any other substrate verb. A per-hart `AtomicBool` reentrancy guard in the emitter forecloses future regressions: re-entry from inside emit is treated as overflow.

**OBS-12. Cross-yield safety.** Trace records, span guards, ActiveWait annotations, and resume payloads must not contain `Witness`, `IdentRef`, `epoch::Guard`, reservation guards, borrowed user slices, or guard-bound references. Trace types carry only `Cap<T>::trace_id() -> u64`, `OperationalEvidence`, owned descriptors, and plain data. This matches [`YIELD-1`](../design/02_execution/REACTOR_v0.md) from the runtime spec.

**OBS-13. Trace-cross types are `Pod`-only.** Every type reachable from `TxTraceRecord` and `Payload*` is `#[repr(C)]` and `unsafe impl tx_hal::Pod`. No `Debug`, `Serialize`, or `Display` derives on the kernel side. Host-side daemon enables these via the `host` Cargo feature on `tx-observe-types`.

## 8. MVP scope

<!-- txdoc:OBS-V1-MVP-1 -->

### 8.1 In scope

- L0 syscall enter/exit via `traced_syscall!` macro in `tx-shims/src/adapter.rs`.
- L2 drive begin/end inside `tx-scripts::drive`.
- L4 step SpanBegin/SpanEnd around `op.step(ctx)` call site, with `PayloadStepOutcome` (variant, progress_kind, **progress_value**, errno, shape_kind).
- L3 yield begin/resume hook code wired (will fire once reactor parking lands; see [§15](#15-open-questions)).
- L3 producer-side flow record on `WaitSource::notify` (pulled forward from prior deferred lists; cost is one record per notify).
- `txtrace-v0` wire format ([companion](08_OBSERVATION_SERIALIZATION_v0.md)) implemented and exercised.
- Per-hart ring backed by `HartLocal<HartEmitter>` and `ObserverIf::observation_ring`.
- ivshmem `ObserverIf` impl on `tx-hal-riscv64-qemu-virt`.
- Host daemon decode + Perfetto `.pftrace` emission for the in-scope event set.

### 8.2 Out of scope

- L1 Script-level (multi-drive syscall envelope) — wait for execve / ptrace pressure.
- L5 Phase-level (observe/upgrade/reserve/commit/publish boundaries) — needs `RawTrace<P>` subscriber wiring; defer until step bodies stabilize.
- L6 Mutation-level (`zone::sign`, `index::commit` instants) — needs per-substrate hook placement; defer.
- Full `OnAgent` round-trip flow visualization (agent dequeue, reply install) — emit-side hooks exist but daemon-side track lifecycle for endpoints is L3b work.
- Dynamic string interning (path strings, comm names) — out-of-band `names.json` is sufficient for MVP.
- Process_tree packet emission — daemon shows pid/tid as annotations only.
- Compression (`.zst`) for offline traces.
- Live Perfetto producer-SDK mode — `.pftrace` file output is sufficient.

### 8.3 Deferred but pre-wired

- L3 Yield/Resume emit code lands in OBS-3b (in `tx-scripts::drive`'s yield/resume branches). The code compiles, the events register through level gates, but they don't fire until [`drive`'s `AcceptOutcome::Resolve` branch](../../crates/tx-scripts/src/drive.rs) wires reactor parking. When parking lands, observation lights up without observation-side changes.

OBS-V1-MVP-RATCHET: MVP delivers strace + sched-flame for syscall debug. "Why is Continue spinning?" answerable by stepping through L4 SpanEnd payloads with progress_value. "Why didn't Yield resume?" answerable by the L3 producer-side flow record (kernel emits the notify even when the daemon's consumer hasn't joined the flow yet — diagnostic value retained).

## 9. Worked example: sys_read

<!-- txdoc:OBS-V1-EXAMPLE-1 -->

A `read(3, 0x10000, 4096)` against a pipe with no data, then a writer commits 4096 bytes from another hart.

Records emitted (hart 0 is the reader; hart 3 is the writer):

```text
hart 0:
  SpanBegin level=L0 name=sys_read  payload=SyscallEnter{sysno=READ, abi=LinuxRv64, argc=3}
  ArgCont   span=sys_read key=fd  value=3
  ArgCont   span=sys_read key=buf value=0x10000
  ArgCont   span=sys_read key=len value=4096
  SpanBegin level=L2 name=PipeReadOp  payload=DriveBegin{mode=Waiting, interrupt=Interruptible}
    SpanBegin level=L4 name=step.iteration_0
    SpanEnd   level=L4 payload=StepOutcome{variant=Yield, progress_value=0, shape_kind=OnWaitSource}
    SpanBegin level=L3 name=yield.OnWaitSource
              payload=YieldBegin{shape_kind=OnWaitSource, task_id_low=42,
                                 wait_generation=7}

hart 3 (writer notifying):
  Instant level=L3 name=wake.notify
          payload=WaitSourceNotify{source_id=PIPE_READ_SOURCE_42, mask_bits=HasData,
                                   flow_id_source=(task_id=42, wait_gen=7, kind=SourceWake)}

hart 0 (resumed):
    Instant   level=L3 name=resume  payload=Resume{resume_kind=Retry, wait_generation=7}
    SpanEnd   level=L3
    SpanBegin level=L4 name=step.iteration_1
    SpanEnd   level=L4 payload=StepOutcome{variant=Done, progress_value=4096}
  SpanEnd level=L2 name=PipeReadOp
  SpanEnd level=L0 name=sys_read  payload=SyscallExit{result_kind=Ok, ret=4096}
```

Daemon reconstruction renders:
- One async slice "sys_read(fd=3, buf=0x10000, len=4096)" on hart 0's task track, 2ms duration.
- Two nested sync slices for step iterations, with progress_value=0 and progress_value=4096.
- One async slice "yield.OnWaitSource gen=7" inside the drive, with a flow arrow from hart 3's `wake.notify` instant to hart 0's `resume` instant. The arrow makes the wake source visible.
- Result "ret=4096" annotation on the sys_read slice's end.

This view is sufficient for: "what syscall, what args, how many step iterations, did it block on read, what woke it, how much got moved, what's the latency budget."

## 10. Per-hart static placement

<!-- txdoc:OBS-V1-PERHART-1 -->

The per-hart emitter lives in a `static HartLocal<HartEmitter>` ([`tx-hal::HartLocal`](../../crates/tx-hal/src/hart_local.rs)):

```rust
// tx-observe/src/emitter.rs
use tx_hal::{CpuId, HartLocal, PercpuIf};

static EMITTERS: HartLocal<HartEmitter> = HartLocal::new();

pub struct HartEmitter {
    ring: SpscRing,                            // built over ObserverIf::observation_ring()
    seq: AtomicU64,                            // per-hart monotone
    reentry_guard: AtomicBool,                 // OBS-11
}

pub fn observation_boot_hart<P: ObserverIf + PercpuIf>(hart: CpuId) {
    if let Some(desc) = P::observation_ring(hart) {
        EMITTERS.init(hart, HartEmitter::new(desc));
    }
    // else: no transport on this board/hart; emit becomes no-op.
}

#[inline]
fn current_emitter<P: PercpuIf>() -> Option<&'static HartEmitter> {
    EMITTERS.get::<P>()
}
```

OBS-V1-PERHART-INIT: `observation_boot_hart::<P>(hart)` is called once per hart during kernel init, after `PercpuIf::install_early_percpu` returns. Boot order is `early_percpu → observation_boot_hart → first emit-eligible code path`. If observation is feature-disabled, `observation_boot_hart` is a no-op.

OBS-V1-PERHART-REENTRY: the `reentry_guard` is set with CAS before each emit and cleared after. If the CAS fails (we're already inside emit), the record is dropped and `lost` is incremented. Forecloses OBS-11 at runtime.

## 11. Op-name policy

<!-- txdoc:OBS-V1-OPNAME-1 -->

`StepOp` implementations are named for trace purposes via `core::any::TypeId::of::<O>()`, truncated to `u32`:

```rust
// tx-observe/src/intern.rs
use core::any::TypeId;

#[inline]
pub fn op_name_id<O: 'static>() -> u32 {
    // Truncate TypeId's u128 hash to u32. Collision-resistant against
    // accidental aliasing across the ~178 production StepOp impls
    // (per docs/Txv3/07_BLAST_RADIUS.md §3.2).
    let tid = TypeId::of::<O>();
    let bytes: [u8; 16] = unsafe { core::mem::transmute(tid) };
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}
```

Stable per kernel binary. Daemon resolves `EventNameId → human name` from a build-emitted `names.json` keyed by `boot_id`. The names file is generated by an `xtask` that scans the kernel ELF's symbol table for `impl StepOp for X` instantiations and emits a `{ "u32_id": "tx_subsystems::pipe::execution::PipeReadOp" }` table.

OBS-V1-OPNAME-NO-TRAIT-METHOD: this scheme does *not* add an `op_name()` method to `StepOp` ([`03_STEP_MODEL_v2` §3](03_STEP_MODEL_v2.md)). The trait remains exactly as v2 specifies. Subagents implementing v2 do not need to know about observation.

OBS-V1-OPNAME-FUTURE: a build-script-generated stable cross-build registry is reserved as future work if cross-build trace comparisons become useful.

## 12. Object identifiers

<!-- txdoc:OBS-V1-OBJECT-IDS-1 -->

Zone-backed objects are identified in trace by a packed 64-bit form:

```text
TraceObjectId (u64) layout:
  bits  0..32  slot index           (32 bits = 4G slots per zone)
  bits 32..56  generation           (24 bits; rolls over on slot reuse)
  bits 56..64  kind tag             (8 bits = 256 zone kinds)
```

Encoded once at the `Cap<T>` boundary:

```rust
// tx-substrate/src/zone/cap.rs (or verbs.rs re-export)
impl<T: ZoneKindTag> Cap<T> {
    /// Pack (kind, generation, slot) into a u64 for trace emission.
    /// Caller must already hold the Cap (so the generation is live).
    #[inline]
    pub fn trace_id(&self) -> u64 {
        ((T::KIND as u64) << 56)
            | ((self.generation() as u64 & 0xFF_FFFF) << 32)
            | (self.slot_index() as u64)
    }
}
```

`ZoneKindTag` is a simple per-zone-type associated constant:

```rust
pub trait ZoneKindTag {
    const KIND: u8;
}
```

OBS-V1-OBJECT-ENCODING: 24 bits of generation handles ~16M reuses per slot before wraparound. At a sustained slot-reuse rate of 1Hz, that's 6 months; under load it's still ample. Daemon detects the rollover by `boot_id` change and treats reused-slot collisions as new identities.

OBS-V1-OBJECT-COMPAT: the txtrace-v0 spec's `PayloadArgCont::value0: u64` carries `TraceObjectId` directly. The 16-byte `TxObjectId` form in [`txtrace-v0 §14`](08_OBSERVATION_SERIALIZATION_v0.md#14-object-identifiers) is reserved for a future blob-payload variant and is not used in v1.

## 13. Span and flow id namespaces

<!-- txdoc:OBS-V1-IDS-1 -->

### 13.1 Span IDs

Per-hart u64 counter with the hart_id encoded in the high bits:

```text
TraceSpanId (u64) layout:
  bits  0..56  per-hart monotone counter
  bits 56..64  hart_id
```

Encoded in the emitter:

```rust
impl HartEmitter {
    fn mint_span_id(&self, hart: CpuId) -> u64 {
        // span_counter is kernel-private, distinct from `TxTraceHartRing.seq`
        // (which is the per-record sequence number). Conflating them would
        // mean every emit consumed a span-id slot, depleting the 2^56 budget
        // at the rate of every event rather than every span begin.
        let local = self.span_counter.fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1); // 1-based so counter==0 stays sentinel for SpanId::NONE.
        ((hart.0 as u64) << 56) | (local & 0x00FF_FFFF_FFFF_FFFF)
    }
}
```

OBS-V1-SPAN-NAMESPACE: cross-hart span uniqueness is guaranteed by the hart_id high byte. The daemon disambiguates by reading the high byte; never treats raw u64 span IDs as globally unique without consulting it.

OBS-V1-SPAN-COUNTER-PRIVATE: the span-id local counter is kernel-private — stored in `HartSlot::span_counter` (in-RAM only, never serialised). Only the materialised `SpanId` appears on the wire, in `TxTraceRecord.span` and `.parent`. The wire `TxTraceHartRing.seq` is unrelated: it is the per-record sequence number, used by the daemon for loss-detection and ordering. Implementers must keep the two counters separate; collapsing them re-introduces the bug caught during OBS-2.

### 13.2 Flow IDs

The kernel emits `(task_id, wait_generation, flow_kind)` material in producer and consumer events. The daemon computes the Perfetto flow_id via:

```rust
// tools/tx-trace-daemon/src/reconstruct/flow.rs
fn compute_flow_id(task_id: u32, wait_gen: u64, kind: FlowKind, boot_id: u64) -> u64 {
    // SipHash-1-3 with per-boot key derived from boot_id.
    let mut hasher = SipHasher13::new_with_keys(boot_id, !boot_id);
    (task_id, wait_gen, kind as u8).hash(&mut hasher);
    hasher.finish()
}

#[repr(u8)]
enum FlowKind {
    SourceWake    = 1,  // WaitSource::notify → resume
    AgentReply    = 2,  // DelegateToken::reply → resume
    TimerExpire   = 3,  // TimerToken expiry → resume
    AbortDelivery = 4,  // generationless abort → resume
}
```

OBS-V1-FLOW-HASH: SipHash with per-boot key prevents adversarial collisions in shared traces. Plain xor-fold is too weak for traces uploaded to shared infrastructure.

OBS-V1-FLOW-DAEMON-OWNS: the kernel never computes flow_ids. The kernel emits the *material* (`task_id_low`, `wait_generation`, `shape_kind` for producer / resume_kind for consumer). The daemon hashes.

## 14. Substrate cost

<!-- txdoc:OBS-V1-COST-1 -->

| Component | LoC | Crate / file |
|---|---|---|
| `ObserverIf` trait + `RingDescriptor` | ~80 | `tx-hal/src/observer.rs` |
| `tx-observe-types` (all wire-format structs) | ~300 | new crate |
| `tx-observe` core (ring, emitter, intern, boot) | ~400 | new crate |
| ivshmem impl on `rv64-qemu` | ~150 | `boards/tx-hal-riscv64-qemu-virt/src/observer.rs` |
| Other board stubs | ~20 each | `boards/tx-hal-*/src/observer.rs` |
| Drive-loop instrumentation | ~50 | `tx-scripts/src/drive.rs` (additive) |
| Shim wrapper macro | ~100 | `tx-shims/src/adapter.rs` (additive) |
| `WaitSource::notify` producer hook | ~10 | `tx-substrate/src/wake/...` (additive) |
| `Cap::trace_id` + `ZoneKindTag` | ~50 | `tx-substrate/src/zone/cap.rs` |
| Host daemon (MVP: decode + JSON + .pftrace) | ~1500 | `tools/tx-trace-daemon/` |
| **Total kernel-side** | **~1160** | |
| **Total host-side** | **~1500** | |

OBS-V1-COST-COMPARE: kernel-side ~1160 LoC vs the AIO subsystem's ~2000+ LoC ([`06_EXECUTION_SCOPE_v1 §8.2`](06_EXECUTION_SCOPE_v1.md)) and the wake-substrate's ~1500 LoC ([`07_BLAST_RADIUS §4`](07_BLAST_RADIUS.md) F row). Observation is a comparable-sized feature, smaller than either prior PR-11 / PR-3 increments.

## 15. Open questions

<!-- txdoc:OBS-V1-OPEN-1 -->

15.1. **Reactor parking in `drive::AcceptOutcome::Resolve`.** ([`crates/tx-scripts/src/drive.rs`](../../crates/tx-scripts/src/drive.rs) currently returns `EAGAIN` from the Resolve arm.) L3 Yield/Resume hook code lands in OBS-3b but does not fire until reactor parking lands. Not blocking OBS-0 through OBS-3a. When parking lands, observation lights up automatically; verify with a pipe-EOF test that exercises the Yield path.

15.2. **L5 Phase tracing through `RawTrace<P>`.** [BUS_v1](../design/01_substrate/BUS_v1.md) defines `RawTrace<P>::emit(payload)` as nop-patched without subscribers. The observation subsystem is the natural first subscriber. Wiring is straightforward but requires per-substrate trace points to be declared inside step bodies (the only OBS site allowed inside step code per OBS-V1-NO-STEP-BODY). Out of MVP; revisit after step-body stability surveys.

15.3. **Per-cgroup / per-namespace track scoping.** When containers/namespaces land ([`05_DELEGATE_v1.md`](05_DELEGATE_v1.md) gestures at this), observation tracks may want a per-namespace filter at the daemon. Reserved for the cgroup work.

15.4. **Dynamic string interning.** v1 cannot emit path strings, comm names, or any data not known at compile time. The deferred `ArgBlob` continuation variant (multi-record blob payload) is the path forward. Specification reserved in [`08_OBSERVATION_SERIALIZATION_v0.md §13.x`](08_OBSERVATION_SERIALIZATION_v0.md).

15.5. **OnAgent agent-side tracks.** Visualizing FUSE/ufd/ptrace agent processes as their own Perfetto tracks ("the FUSE daemon was here while sys_read was yielded") needs daemon-side track lifecycle management for endpoints. Deferred from MVP; daemon stubs the agent-side as anonymous events.

15.6. **Reactor scheduler track (sched_switch-equivalent view).** Maps which reactor task held which hart as a Gantt timeline. Requires reactor-side hooks at task dispatch/yield/park, which depend on parking landing first. Deferred to OBS-9 (post-MVP).

15.7. **Compression for offline `.pftrace` files.** Perfetto UI accepts zstd-compressed inputs. ~30 LoC daemon-side addition. Out of MVP; trivial to add.

## 16. Migration order

<!-- txdoc:OBS-V1-MIGRATION-1 -->

Sequential PRs. OBS-0 through OBS-2 are pure additions (no existing-file changes except adding `ObserverIf` to the `TxPlatform` supertrait bundle). OBS-3 onward is gated by compile features defaulting to off.

| PR | Lands | Touches | Risk |
|---|---|---|---|
| **OBS-0** | `tx-hal::observer` module + per-board stub impls returning `None` | `tx-hal/src/{observer.rs, lib.rs}`, three board crates | Low — additive trait, default impls |
| **OBS-1** | `tx-observe-types` crate with all wire-format structs | new crate; workspace member addition | Low — no consumers yet |
| **OBS-2** | `tx-observe` crate; per-hart emitter; no-op stubs by default | new crate; workspace member | Low — additive |
| **OBS-3a** | L0/L2/L4 emit calls inside `tx-scripts::drive` + `traced_syscall!` macro in shims; ivshmem on rv64-qemu | `tx-scripts/src/drive.rs`, `tx-shims/src/adapter.rs`, `boards/tx-hal-riscv64-qemu-virt/src/observer.rs` | Medium — first behavior change (feature-gated); pipe/read smoke test |
| **OBS-3b** | L3 Yield/Resume emit calls inside `drive` (deferred-firing) | `tx-scripts/src/drive.rs` | Low — code compiles but inert until reactor parking |
| **OBS-4** | Producer-side `WaitSource::notify` flow record + `Cap::trace_id` + `ZoneKindTag` | `tx-substrate/src/wake/`, `tx-substrate/src/zone/cap.rs` | Low — one record per notify |
| **OBS-5** | Host daemon: mmap + decode + names.json + JSON dump | `tools/tx-trace-daemon` | Low — host-side only |
| **OBS-6** | Host daemon: Perfetto `.pftrace` emission | `tools/tx-trace-daemon` | Low — host-side only |
| **OBS-7** | Boundary lint extension: forbid `tx_observe::emit_*` inside `#[platform_adapter]`-marked modules | `xtask/src/boundary_report.rs` | Low — accounting check |
| **OBS-8** | (deferred from MVP) L5 phase / L6 mutation hook landings | per-subsystem files; `tx-substrate/src/zone/`, `tx-substrate/src/index.rs` | Low per-site |
| **OBS-9** | (deferred) Reactor scheduler track | `tx-reactor/src/scheduler.rs`, daemon track-lifecycle | Medium |

Total in-scope (OBS-0 through OBS-7): **~6 working days kernel-side, ~3 working days daemon-side, ~9 days total**.

OBS-0/1/2 can land in parallel (independent crates). OBS-3a depends on all three. OBS-4 can land in parallel with OBS-3a. OBS-5/6 are sequential. OBS-7 is post-OBS-3a (needs adapters to know about emit calls).

## 17. Anti-patterns

<!-- txdoc:OBS-V1-ANTI-1 -->

| ID | Anti-pattern | Violates | Fix |
|---|---|---|---|
| OBS-A-1 | Emitting a trace record from inside a `StepOp::step()` body. | OBS-V1-NO-STEP-BODY | Move to `drive` wrapper, or use `RawTrace<P>` (L5 only). |
| OBS-A-2 | Emitting a trace record from inside an adapter verb. | OBS-V1-CONVERGENCE | Adapter verbs call substrate; substrate emits. |
| OBS-A-3 | Dereferencing a `UserPtr<T>` to format a trace argument. | OBS-2 | Emit raw pointer as `u64`; let shim emit decoded form after copy. |
| OBS-A-4 | Calling `format_args!` / `Debug::fmt` to construct a name. | OBS-4 | Use static `EventNameId`; resolve human name in daemon. |
| OBS-A-5 | Allocating in the emit path (`Vec::push`, `String::new`, etc.). | OBS-3 | Use the per-hart fixed-size ring slot directly. |
| OBS-A-6 | Storing a `Witness`, `IdentRef`, or epoch `Guard` in a trace record or span guard. | OBS-12 | Pack `Cap::trace_id()` instead. |
| OBS-A-7 | Calling `tx_observe::emit_*` from inside `tx_observe::emit_*` (e.g., emit a "zone signed" record from emit's allocation path). | OBS-11 | Per-hart reentrancy guard catches at runtime; design fix is to make emit truly allocation-free. |
| OBS-A-8 | Using raw `WaitGeneration` as a global flow id. | OBS-V1-FLOW-DAEMON-OWNS | Kernel emits material; daemon hashes. |
| OBS-A-9 | Deriving `Debug` / `Serialize` on a `TxTraceRecord` or `Payload*` struct in the kernel-side build. | OBS-13 | Use `tx-observe-types`'s `host` feature for daemon use. |
| OBS-A-10 | Variable-length records inside the main ring. | OBS-10, OBS-SER-10 | Use fixed-size records with `ArgCont` continuations. |

## 18. Cross-references

<!-- txdoc:OBS-V1-XREFS-1 -->

- Wire-format ABI: [`08_OBSERVATION_SERIALIZATION_v0.md`](08_OBSERVATION_SERIALIZATION_v0.md).
- Daemon reconstruction: [`08_OBSERVATION_HOST_v0.md`](08_OBSERVATION_HOST_v0.md).
- Closed forward references:
  - [`SCHED-7-4-OBSERVATION-SUBSYSTEM-FUTURE`](../design/02_execution/SCHEDULER_v0.md) — Phase 2 observation subsystem.
  - [`STEP-11`](../design/02_execution/STEP_MODEL_v1.md) — step-local tracing.
  - [`THREAD_RUNTIME_v1`](../design/02_execution/THREAD_RUNTIME_v1.md) "observation subsystem (future)" note.
- Substrate convergence points:
  - [`Reactor_concept_v5_RefactorSpec v4.md`](../design/02_execution/REACTOR_v0.md) — `WaitGeneration`, `TaskMailbox`, `WaitSource`, `DelegateToken`.
  - [`03_STEP_MODEL_v2.md`](03_STEP_MODEL_v2.md) — `StepOp`, `StepOutcome`, `YieldShape`.
  - [`BUS_v1.md`](../design/01_substrate/BUS_v1.md) — `RawTrace<P>` (L5 hook).
- Implemented code:
  - [`tx-scripts::drive`](../../crates/tx-scripts/src/drive.rs) — central driver, future hook site.
  - [`tx-hal::HartLocal`](../../crates/tx-hal/src/hart_local.rs) — per-hart slot primitive.
  - [`tx-substrate::verbs`](../../crates/tx-substrate/src/verbs.rs) — curated substrate verb namespace.

## 19. Observation tooling and test helpers

<!-- txdoc:OBS-V1-TOOLING-1 -->

### 19.1 `cargo xtask observe` — pipeline wrappers

`cargo xtask observe` wraps the `tx-trace-daemon` binary so humans and CI can
drive the observation pipeline without memorising daemon CLI flags.

| Subcommand | Description |
|---|---|
| `observe demo --output <path>` | Generate a synthetic `.txtrace` file with a known mix of records (SpanBegin/End, Instant, WaitSourceNotify, Counter; optionally YieldBegin/Resume with `--with-yields`). No daemon required — pure Rust byte writer. |
| `observe validate --file <path>` | Parse the file header, walk slots, print a one-line summary: `txtrace v0: 1 hart, 16 slots/hart, 4 records, 0 framing errors`. Fast CI smoke check. |
| `observe replay --file <path> [--out json\|pftrace] [--output <path>] [--filter level=N]` | Decode a `.txtrace` file. Default `--out json` writes NDJSON to stdout (pipeable to `jq`). With `--out pftrace --output <path>` writes a Perfetto `.pftrace` file. |
| `observe pftrace --file <path> --output <pftrace>` | Convenience alias for `replay --out pftrace`; avoids remembering two flags for the common Perfetto case. |

The daemon CLI surface is documented in [`08_OBSERVATION_HOST_v0.md`](08_OBSERVATION_HOST_v0.md).

### 19.2 `tx_observe::testing` — test helpers

`crates/tx-observe/src/testing.rs` (enabled via `features = ["testing"]` or
`cfg(test)`) provides `TestPlatform` / `TestObservation` — a RAII harness that
eliminates the ~150-line `TestPlatform` boilerplate currently repeated in every
observation integration test.

```ignore
use tx_observe::testing::TestPlatform;

#[test]
fn my_test() {
    let obs = TestPlatform::new().init();
    obs.emitter().instant(/* ... */);
    let records = obs.records();
    assert_eq!(records.len(), 1);
}
```

`TestPlatform::init()` acquires a crate-level `Mutex` that serialises all tests
using the helpers, preventing concurrent races on the observation statics.
`TestObservation`'s `Drop` resets `TS_FN`, `CPU_ID_FN`, `HART_SLOTS`, and
`EMITTERS` for the test's hart so the next test starts clean.

Migration reference: `crates/tx-observe/tests/smoke.rs` was migrated from
~337 lines to ~100 lines using these helpers.
  - [`tx-hal::Pod`](../../crates/tx-hal/src/lib.rs) — POD marker reused for trace records.
