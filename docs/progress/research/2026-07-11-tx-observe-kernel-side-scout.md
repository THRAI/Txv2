# tx-observe kernel-side schema scout

Date: 2026-07-11

Scope: read-only scout of kernel-side `tx-observe` users after landing the
L0 `schema/txobserve.toml` inventory and `cargo xtask observe-schema check`
gate. The goal is to decide whether the next slice should migrate kernel
callers to typed schema-backed APIs or first extend the schema with more
features.

## Summary

The schema side is ready for kernel-side scouting, but not yet ready for
generation. The current TOML/checker has enough mechanical truth for ABI,
payload, cfg, projection, and explicit-track drift checks. The remaining
schema gap is the concrete event/name catalog: kernel callsites still create
many `EventNameId`s from raw hashes or literals, and host tooling still relies
on `KERNEL_FNV1A_STABLE_NAMES`, analyzer-side source harvesting, and fallback
`names.json` generation.

The kernel-side code falls into two very different categories:

1. Structured convergence events are few and already close to the target L1/L2
   shape. These are syscall, drive, step, yield/resume, wake notify, phase, and
   mutation records.
2. Debug counter names are broad and scattered. A source scan found 641
   distinct `debug.*` strings under the kernel-side crates scanned
   (`tx-kernel`, `tx-shims`, `tx-scripts`, `tx-substrate`, `tx-reactor`,
   `tx-subsystems`, and `tx-observe`). These should not all become handwritten
   per-event Rust APIs before the schema grows a real stable-name catalog.

## Current raw-boundary map

| Area | Current shape | Schema mapping | Migration readiness |
|---|---|---|---|
| L0 syscall boundary | `crates/tx-shims/src/linux_syscall/mod.rs` builds `PayloadSyscallEnter`, emits `ArgValue` continuation instants, closes with `PayloadSyscallExit`; older `traced_syscall!` macro still has a simpler `argc=3` shape | `event_families.syscall`, `payloads.syscall_enter`, `payloads.arg_value`, `payloads.syscall_exit` | Ready for a typed `syscall_enter/arg/exit` helper. Needs one compatibility decision for the older macro path. |
| L2 drive | `crates/tx-scripts/src/drive.rs` builds `PayloadDriveBegin/End`, hashes `type_name::<S>()`, and threads parent spans with `set_current_parent_span` | `event_families.drive`, `payloads.drive_begin`, `payloads.drive_end` | Ready for a typed drive helper. Dynamic type-name ids need a schema rule for generated `names.json`, not a static event list. |
| L4 step | `drive.rs` opens `step` spans and closes with `PayloadStepOutcome` | `event_families.drive`, `payloads.step_outcome` | Ready. Static `step` name can be a schema-owned stable name. |
| L3 yield/resume | `drive.rs` uses fixed names `yield.OnWaitSource`, `yield.OnAgent`, `yield.OnTimer`, `resume`, plus `PayloadYieldBegin` and `PayloadResume` | `event_families.yield`, `payloads.yield_begin`, `payloads.resume` | Ready. Names should move into schema stable names. |
| wake notify | `crates/tx-substrate/src/wake/wait_source.rs` repeats `PayloadWaitSourceNotify` construction in three notify paths, using `wake.notify` | `event_families.yield`, `payloads.wait_source_notify` | Ready for an L1 helper. This is one of the cleanest first migrations because the duplicate code is obvious. |
| phase | `crates/tx-substrate/src/lib.rs` builds `PayloadPhaseTransition`, opens/closes phase spans, and uses `phase_kind as u32` as name | `event_families.phase`, `payloads.phase_transition` | Mostly ready. Name semantics should be schema-owned so boot phase names are stable and readable. |
| mutation | `crates/tx-substrate/src/zone/reservation.rs` and `crates/tx-substrate/src/index.rs` build mutation payloads and use raw literal ids `0x4d5a5347` and `0x4d494358` | `event_families.mutation`, `payloads.mutation_zone_sign`, `payloads.mutation_index_commit` | Ready for typed helper. The literal ids should become schema stable names (`mutation.zone_sign`, `mutation.index_commit`) or generated constants. |
| process/sched labels | `crates/tx-scripts/src/process/exec/script.rs` builds `PayloadProcessLabel/Group` and uses synthetic raw ids `0x9000_0000 | pid` and `0xA000_0000 | pid` | `event_families.scheduler`, `payloads.process_label`, `payloads.process_group` | Needs design before API migration. These are per-process synthetic ids, not plain stable names. Schema should model id derivation. |
| counters | many helpers call `observer.counter(EventNameId::from_raw(fnv1a32(name)), value)` | many name families; mostly `counter_value` payload | Not ready for per-event generated APIs. First add schema support for concrete stable names and wildcard/dynamic families. |
| allocation | `observer.allocation(track, EventNameId::from_raw(fnv1a32(name)), value)` uses explicit tracks and `ArgValue` payloads | `tracks.explicit`, `event_families.allocation`, `payloads.arg_value` | Track ids are ready; allocation name catalog is not fully concrete. |
| lock metrics | `tx_substrate::SpinMutex<T, LockMetricsOn>` stores a hashed lock name and emits metric names such as `debug.lock.wait_ns` | `controls.groups.lock_metrics`, `tracks.explicit.lock`, `names.families.lock` | Type boundary is good. Schema should own lock metric names and lock-name family policy. |
| DS method metrics | substrate/process DS metrics hash method names and emit `debug.ds.method.duration_ns`, optionally with `debug.ds.method.zone_id` | `controls.groups.ds_metrics`, `tracks.explicit.ds_method`, `names.families.ds_method` | Type boundary is good. Schema should model dynamic method-name families. |

## Main observations

1. `tx-observe` itself is the only legitimate place that currently constructs
   `TxTraceRecord`. The risky public surface is not record construction at
   random callsites; it is that `HartEmitter::{span_begin, span_end, instant}`
   still accept `(TxPayloadTag, &[u8])`.

2. `crates/tx-observe/src/encode.rs` is already the natural L3 seam. It binds
   each payload struct to byte layout and tag helper functions. The next code
   migration should wrap these helpers instead of replacing them.

3. `SpanId` is currently the cross-layer capability. Callers can close a span
   with a naked `SpanId`, and `span_begin` returns a `SpanId` even if the
   underlying `emit` drops due to ring overflow. This confirms the prior review:
   L2 should grow `EmitStatus`, `DropReason`, and `PublishedSpan` before hard
   lints.

4. `span_end` currently hardcodes `TxTraceLevel::Boundary` and name 0 for every
   close. That is acceptable as current ABI behavior but incompatible with a
   strong typed boundary. `PublishedSpan` should carry enough close metadata or
   an associated event token.

5. The schema's current `event_families` entries are useful but too coarse for
   codegen. They group payloads and projections, but they do not yet define
   concrete event ids, name derivation, emitted record kind, or whether the name
   is stable, dynamic, synthetic, or type-derived.

6. The immediate codegen blocker is not payload shape. Payload shape is already
   checked. The blocker is event/name identity:
   - static names: `step`, `resume`, `yield.OnWaitSource`, `wake.notify`;
   - raw literal names: `0x4d5a5347`, `0x4d494358`;
   - type-derived names: `type_name::<S>()`;
   - syscall-number names: syscall number as name id;
   - synthetic per-object names: process label/group ids;
   - wildcard debug counter names: hundreds of `debug.*` strings.

## Suggested schema additions before generation

Add a name/event layer under L0 before generating Rust:

```toml
[[names.stable]]
id = "wake_notify"
name = "wake.notify"
hash = "fnv1a32"
owner = "tx-substrate::wake"

[[names.synthetic]]
id = "syscall_number"
kind = "numeric_name_id"
source = "SyscallRequest.nr"
family = "syscall"

[[names.derived]]
id = "drive_op_type"
kind = "rust_type_name_fnv1a32"
source = "core::any::type_name::<S>()"
family = "drive"

[[names.dynamic_family]]
id = "futex_debug"
pattern = "debug.futex.*"
source_scan = true
control_group = "signal_futex_metrics"
```

Then add event declarations separate from payload declarations:

```toml
[[events]]
id = "wake_notify"
family = "yield"
kind = "instant"
level = "yield"
name = "wake_notify"
payload = "wait_source_notify"
producer_api = "wake_notify(source_id_low, mask_bits, task_id_low, wait_generation_low)"
```

This keeps the ABI inventory stable while giving codegen enough information to
produce `EventToken<E>` constants, typed callsite helpers, and lint rules.

## Suggested implementation order

1. Add `names.stable`, `names.synthetic`, `names.derived`, and
   `names.dynamic_family` schema sections. Extend `observe-schema check` to
   compare concrete stable names with `xtask/src/observe.rs`
   `KERNEL_FNV1A_STABLE_NAMES` and analyzer fallback names.

2. Add generated or hand-written-for-now L1 helpers for the clean structured
   families first:
   - `syscall_enter_arg_exit`;
   - `drive_begin/end`;
   - `step_begin/end`;
   - `yield_begin/resume`;
   - `wake_notify`;
   - `phase_transition`;
   - `mutation_zone_sign` / `mutation_index_commit`.

3. Add L2 status/capability types before migrating callsites broadly:
   `EmitStatus`, `DropReason`, `PublishedSpan`. Keep compatibility wrappers
   around raw `SpanId` during the first migration.

4. Only after structured families have moved, add lints for direct raw
   `TxPayloadTag`, `Payload*`, and `SpanId` use outside approved modules.

5. Treat debug counters as a separate phase. They need generated stable-name
   constants or family-scoped helper APIs, not one handwritten function per
   `debug.*` string.

## Good first code slice

The best first code slice is `WaitSourceNotify`:

- three duplicated construction blocks in
  `crates/tx-substrate/src/wake/wait_source.rs`;
- one payload type;
- one stable name (`wake.notify`);
- one record kind (`Instant`);
- one level (`Yield`);
- no span lifecycle complication;
- already covered by `event_families.yield` and `payloads.wait_source_notify`.

This slice would prove the schema-to-L1 helper shape without touching syscall
dispatch or span status yet.

## Verification

Read-only scout plus schema gate:

- `cargo xtask observe-schema check` passed:
  `levels=8 record_kinds=11 payloads=23 payload_structs=20 cfgs=34 projections=9 tracks=16`.
- Source scan found 641 distinct `debug.*` strings under kernel-side crates.
- No code was changed by this scout.
