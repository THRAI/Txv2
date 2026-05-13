# Observation Host Daemon — v0

<!-- txdoc:TXV3-OBSERVATION-HOST-V0 -->

**Status.** v0 (Txv3, 2026-05). Companion to the kernel-side observation framework.
**Purpose.** Specify the host-side daemon `tx-trace-daemon`: transport modes, decoding, span/flow reconstruction, repair markers, name resolution, Perfetto output. The daemon is a trace reconstruction layer, not a byte copier — it turns dumb `txtrace-v0` records into Perfetto tracks, slices, flows, annotations, and damage markers.
**Audience.** Subagents implementing `tools/tx-trace-daemon`. Reviewers evaluating the daemon's MVP.
**Companion documents.**
- [`08_OBSERVATION_v1.md`](08_OBSERVATION_v1.md) — framework + invariants.
- [`08_OBSERVATION_SERIALIZATION_v0.md`](08_OBSERVATION_SERIALIZATION_v0.md) — wire format.

---

## 1. Role and non-roles

<!-- txdoc:OBS-HOST-V0-ROLE-1 -->

**Role.** The daemon turns `txtrace-v0` records into Perfetto's `TrackEvent` stream and surfaces what the kernel emitted: spans, instants, flows, annotations, counters, damage markers (overflow, attach-late, missing-begin/end), and loss synthesis.

**Non-roles.**
- Not a passive byte copier. It must reconstruct.
- Not a truth source. The kernel emits *facts*; the daemon renders *presentation*.
- Not a kernel debugger. ptrace and panic-time forensics are out of scope.
- Not a producer of authoritative metrics. Counter records are timeline samples; their statistical interpretation is downstream (Perfetto, scripts).

OBS-HOST-V0-PROMISE: kernel emits compact, non-blocking, non-authoritative facts; daemon reconstructs the human-readable trace. Kernel and daemon evolve at different cadences; the daemon may grow new reconstruction logic without kernel changes.

## 2. Workspace placement

<!-- txdoc:OBS-HOST-V0-PLACEMENT-1 -->

```text
tools/
└── tx-trace-daemon/             # std crate; NOT a workspace member
    ├── Cargo.toml
    └── src/
        ├── main.rs              # CLI entry: `tx-trace-daemon decode <args>`
        ├── transport/
        │   ├── mod.rs
        │   ├── ivshmem.rs       # Live mmap of /dev/shm/<region>
        │   ├── file_replay.rs   # Open and replay a captured region
        │   └── snapshot.rs      # Read a memory-backend-file post-mortem
        ├── decode/
        │   ├── mod.rs
        │   ├── header.rs        # TxTraceHeader validation
        │   ├── ring.rs          # SPSC ring drain
        │   └── record.rs        # TxTraceRecord + payload decoding
        ├── reconstruct/
        │   ├── mod.rs
        │   ├── span.rs          # Span table; SpanBegin/SpanEnd reconciliation
        │   ├── flow.rs          # flow_id hashing; producer/consumer pairing
        │   ├── track.rs         # TrackDescriptor → Perfetto TrackDescriptor
        │   └── repair.rs        # LostRecords synthesis; unbalanced-span markers
        ├── names/
        │   ├── mod.rs
        │   ├── embedded.rs      # Parse string_table_off table
        │   └── from_json.rs     # Load names.json keyed by boot_id
        ├── perfetto/
        │   ├── mod.rs
        │   ├── proto.rs         # protobuf encoder (prost-based)
        │   ├── track_event.rs
        │   └── pftrace_writer.rs
        └── output/
            ├── mod.rs
            ├── pftrace.rs       # `.pftrace` file emission
            └── debug_json.rs    # JSON dump for daemon debugging
```

OBS-HOST-V0-NOT-IN-WORKSPACE: `tools/tx-trace-daemon/` is its own cargo project (with its own `Cargo.toml`), not a member of the kernel workspace (`/Cargo.toml`). This keeps host dependencies (`prost`, `serde_json`, `clap`, etc.) out of the kernel build graph.

OBS-HOST-V0-DEPS: depends only on `tx-observe-types` (with the `host` feature enabled). Does not depend on any other kernel crate.

## 3. Transport modes

<!-- txdoc:OBS-HOST-V0-TRANSPORT-1 -->

Three transports, plus a degenerate fourth for testing:

| Mode | CLI flag | Behavior |
|---|---|---|
| Live mmap (ivshmem) | `--live <path>` | Open and mmap the ivshmem region path (`/dev/shm/<name>` or QEMU memory-backend-file). Drain in a loop until interrupted. |
| File replay | `--replay <file>` | Open a captured region snapshot; play through records start-to-end as if live. Used to debug daemon bugs without rerunning QEMU. |
| Snapshot decode | `--snapshot <file>` | Read a captured region from a memory-backend-file post-crash; decode all records in one pass; emit `.pftrace` + halt. |
| Pipe (test) | `--pipe` | Read raw record bytes from stdin. Used by daemon unit tests to feed synthetic record sequences. |

OBS-HOST-V0-TRANSPORT-IVSHMEM: live mmap requires the ivshmem region to be backed by `-object memory-backend-file,id=trace,size=...,mem-path=/tmp/txtrace` so the daemon mmaps the path. Without `memory-backend-file`, the daemon cannot attach to a running guest (ivshmem-plain BARs are not exposed to the host as paths). The ivshmem device on the QEMU board crate is configured with `memory-backend-file` by default.

OBS-HOST-V0-ATTACH-LATE: when the daemon attaches to a live region with `producer > consumer + slot_count`, the daemon was overrun before consuming. Policy: jump `consumer` to `producer - slot_count` (drop overrun), emit a synthetic Perfetto `data_loss` event with `lost_records: u64 = current_lost - 0` (the kernel-counted drops since last attach), then proceed. Do *not* attempt to replay overwritten records.

## 4. Decoding

<!-- txdoc:OBS-HOST-V0-DECODE-1 -->

### 4.0 Region layout

<!-- txdoc:OBS-HOST-V0-LAYOUT-1 -->

A region (live ivshmem mapping or a `--replay` capture file) is laid out as a single contiguous byte sequence:

```text
offset 0                              : TxTraceHeader                          (72 B)
offset rings_off                      : ring 0
                                          .ring_header  (TxTraceHartRing,      208 B)
                                          .slots        ([TxTraceRecord; 1 << ring_order])
offset rings_off + ring_stride        : ring 1
                                          .ring_header
                                          .slots
…
offset rings_off + (hart_count-1) * ring_stride : ring (hart_count-1)
```

`ring_stride = sizeof(TxTraceHartRing) + (1 << ring_order) * sizeof(TxTraceRecord)`. Ring headers and slots are **interleaved per-hart**, not separated into two arrays. The header's `rings_off` field is the offset of ring 0; subsequent rings are at `rings_off + h * ring_stride`. This keeps each hart's producer-side state cacheline-local with the slots it writes.

### 4.1 Header validation

```text
1. mmap region
2. parse TxTraceHeader at offset 0
3. validate:
   - magic == 0x52545854 ("TXTR")
   - version in supported_range (v0 = exactly 0)
   - record_size == 80
   - hart_count <= MAX_HARTS_DAEMON (suggested cap: 256)
   - 1 << ring_order is sensible (4 .. 24 bits)
   - rings_off >= header_len
   - rings_off + hart_count * ring_size <= region_size
4. record clock_id, clock_freq_hz, boot_id for downstream resolution
```

OBS-HOST-V0-HEADER-FAIL: on any validation failure, emit a structured error to stderr (JSON or human format), do *not* attempt partial decode. Kernel emitted a malformed region; the daemon refuses.

### 4.2 Ring drain

For each hart `h`, repeat:

```text
1. p = ring[h].producer.load(Acquire)
2. c = ring[h].consumer.load(Acquire)
3. if p - c > slot_count:
     overrun_lost += (p - c - slot_count)
     c = p - slot_count   // skip overwritten slots
     synthesize_loss_event(h, overrun_lost)
4. while c != p:
     decode_record(&ring[h].slots[c & (slot_count - 1)])
     c += 1
5. ring[h].consumer.store(c, Release)
6. read lost_now = ring[h].lost.load(Acquire)
7. if lost_now > last_lost[h]:
     synthesize_loss_event(h, lost_now - last_lost[h])
     last_lost[h] = lost_now
```

OBS-HOST-V0-DRAIN-FAIRNESS: drain harts round-robin to keep no single hart starving the daemon. With N harts and slot_count of 16K, one full sweep at typical event rates is microseconds.

### 4.3 Record decode

For each record:

```text
1. validate magic == 0x5254
2. validate version == 0
3. dispatch on kind:
   - Nop: skip
   - ClockSnapshot: update clock translation
   - TrackDescriptor: ingest into track table
   - TrackTombstone: age out track
   - StringDescriptor: ingest into intern table
   - SpanBegin: open span in span table
   - SpanEnd: close span; emit Perfetto slice end
   - Instant: emit Perfetto instant
   - Counter: emit Perfetto counter sample
   - ArgContinuation: attach to most-recent span on this hart
   - PanicMarker: emit halt marker; flush; halt daemon
   - Unknown: warn once per kind; skip
4. extract payload by payload_tag; same forward-compat skip for unknowns
```

## 5. Clock model

<!-- txdoc:OBS-HOST-V0-CLOCK-1 -->

```text
- header.clock_id determines unit (TICKS, where ticks = 1/clock_freq_hz seconds)
- ts on each record is in those units
- Same-hart total order: (hart, seq) within one ring
- Cross-hart order: timestamp-derived
- If header.flags & CLOCK_SHARED: cross-hart timestamps trustworthy
- Else: mark cross-hart edges "approximate" with Perfetto annotation
```

OBS-HOST-V0-CLOCK-PERFETTO: emit a `ClockSnapshot` Perfetto packet at trace start with `clock_id = TRACE_CLOCK_BOOT`, `unit_multiplier_ns = 1e9 / clock_freq_hz`. Per-record `ts` values are sent unchanged; Perfetto handles translation at render time.

OBS-HOST-V0-CLOCK-CALIBRATION: on `clock_shared = false`, the daemon emits a `comment` annotation on cross-hart flows: `"approximate cross-hart timing; clock not calibrated"`. Same- hart slices and flows are exact.

## 6. Name resolution

<!-- txdoc:OBS-HOST-V0-NAMES-1 -->

Precedence:

```text
1. If header.string_table_off != 0:
   - Use kernel-embedded string table as canonical source.
2. Else:
   - Load names.json keyed by boot_id from --names-dir <path>.
3. If neither available:
   - Emit numeric IDs as decimal strings (e.g., "EventName#1234567")
     with a one-line warning.
```

OBS-HOST-V0-NAMES-GENERATION: the `names.json` file is produced by an xtask in the kernel build that walks the ELF's symbol table for `impl StepOp for X` instantiations, hashes each `TypeId::of::<X>()` to u32, and emits:

```json
{
  "boot_id": 0,
  "name_table": {
    "1234567": "tx_subsystems::pipe::execution::PipeReadOp",
    "2345678": "tx_subsystems::vfs::step_open"
  },
  "arg_table": {
    "1": "fd",
    "2": "buf",
    "3": "len"
  }
}
```

OBS-HOST-V0-NAMES-MVP: MVP supports out-of-band `names.json` only. Embedded table is reserved schema; OBS-3a does not implement.

## 7. Track registry

<!-- txdoc:OBS-HOST-V0-TRACKS-1 -->

Map kernel `TraceObjectId` and track-descriptor records to Perfetto `TrackDescriptor`:

| Kernel track | Perfetto track | Created on | Removed on |
|---|---|---|---|
| Hart | `process_uuid=1, thread_uuid=hart_id`, name `"hart-N"` | Boot | Never |
| ReactorTask | child of containing hart's process | `TrackDescriptor` record (task_id) | `TrackTombstone` |
| Process (subject) | top-level process | First syscall referencing the pid | Process exit |
| OnBehalfOf scope | child of kthread track | `with_on_behalf_of` enter | Scope exit |
| DelegateEndpoint | top-level "endpoint" track | First request enqueue | Endpoint reclaim |
| TimerWheel | global | Boot | Never |

MVP track set:
- Hart tracks (always-on)
- ReactorTask tracks (created on first event with that task_id)
- Process tracks (from syscall enter)

Deferred:
- OnBehalfOf scope tracks (lands with OBS-9)
- DelegateEndpoint tracks (lands with OBS-9)
- TimerWheel track (lands with OBS-9)
- Reactor scheduler/sched_switch track (lands with OBS-9)

OBS-HOST-V0-TRACKS-MVP-NAMES: process tracks are named by `pid` only (`"pid-1234"`); comm names require dynamic strings that v0 cannot emit. Full `process_tree` packet is deferred.

## 8. Span reconstruction

<!-- txdoc:OBS-HOST-V0-SPANS-1 -->

The daemon maintains a span table keyed by `(hart, span_id)`:

```rust
struct SpanEntry {
    name: u32,
    level: TxTraceLevel,
    begin_ts: u64,
    track_uuid: u64,
    parent: Option<u64>,
    args: Vec<PayloadArgValue>,
}

struct SpanTable {
    by_hart: HashMap<(u16, u64), SpanEntry>,
}
```

Discipline:

```text
SpanBegin record:
  - insert SpanEntry into table keyed by (hart, span_id)
  - remember `arg_count` so we know how many ArgContinuations to expect

ArgContinuation record:
  - append PayloadArgValue to most-recent SpanEntry on this hart
    that still has arg_count > args.len()

SpanEnd record:
  - lookup (hart, span_id)
  - if found: emit Perfetto slice with [begin_ts, end_ts] + args + payload
  - if not found: emit a "synthetic-span-end" repair marker (an orphan end)
```

OBS-HOST-V0-SPANS-MISSING-BEGIN: if SpanEnd arrives for an unknown (hart, span_id), the daemon emits a Perfetto `Instant` with category `txtrace.repair.orphan_end` and continues. Likely cause: kernel began the span before daemon attached (attach-late).

OBS-HOST-V0-SPANS-MISSING-END: if a SpanBegin is in the table for more than `MAX_OPEN_SPAN_DURATION_NS` (default: 60s of trace time) with no matching End, the daemon flushes it as a `truncated` slice with end_ts = last seen ts, category `txtrace.repair.unbalanced_begin`. Likely cause: kernel panicked or trace ring overflowed mid-span.

## 9. Flow reconstruction

<!-- txdoc:OBS-HOST-V0-FLOWS-1 -->

Flow_ids are computed by the daemon, not the kernel ([`08_OBSERVATION_v1.md §13.2`](08_OBSERVATION_v1.md)):

```rust
fn compute_flow_id(task_id: u32, wait_gen: u64, kind: FlowKind, boot_id: u64) -> u64 {
    let mut h = siphasher::sip::SipHasher13::new_with_keys(boot_id, !boot_id);
    task_id.hash(&mut h);
    wait_gen.hash(&mut h);
    (kind as u8).hash(&mut h);
    h.finish()
}

#[repr(u8)]
enum FlowKind {
    SourceWake    = 1,
    AgentReply    = 2,
    TimerExpire   = 3,
    AbortDelivery = 4,
}
```

Discipline:

```text
WaitSourceNotify record (producer):
  - compute flow_id = hash(task_id, wait_gen, SourceWake, boot_id)
  - emit Perfetto Instant with flow_id

YieldBegin record (consumer enters):
  - same flow_id (terminating)
  - attach to the yield span; Perfetto draws the arrow

Resume record:
  - close terminating flow_id
```

OBS-HOST-V0-FLOWS-COLLISION: SipHash with per-boot key prevents adversarial collisions in shared traces. Accidental collisions on `(task_id, wait_gen, kind)` are statistically negligible given the 64-bit output.

OBS-HOST-V0-FLOWS-NO-RAW-WAIT-GEN: the daemon must never use raw `wait_generation` as the global flow_id. It's per-task; would alias across tasks. The hash discipline is mandatory.

OBS-HOST-V0-FLOWS-AGENT: OnAgent round-trips produce *two* flow arrows: script→endpoint (kind=AgentReply, generated by the kernel when reply lands) and endpoint→script (same kind, terminating). Daemon-side endpoint track is deferred; in MVP, AgentReply flows render as arrows between two task tracks.

## 10. Repair markers and loss

<!-- txdoc:OBS-HOST-V0-REPAIR-1 -->

The daemon synthesizes repair events into the Perfetto stream with a dedicated category prefix `txtrace.repair.*`:

| Scenario | Repair marker category | Visualization |
|---|---|---|
| Ring overflow (producer outpaced consumer) | `txtrace.repair.lost_records` | Instant on the affected hart's track; arg `count = N` |
| Daemon attached late, missed prior events | `txtrace.repair.attach_late` | Instant at attach time; arg `skipped_records = N` |
| SpanBegin with no matching SpanEnd within timeout | `txtrace.repair.unbalanced_begin` | Truncated slice; arg `original_begin_ts` |
| SpanEnd with no matching SpanBegin | `txtrace.repair.orphan_end` | Instant; arg `orphan_span_id` |
| Unknown record kind | `txtrace.repair.unknown_kind` | Instant; arg `kind_value`; emitted once per kind per drain pass |
| Unknown payload tag | `txtrace.repair.unknown_payload` | Instant; arg `tag_value` |
| Timestamp anomaly (same-hart backwards) | `txtrace.repair.clock_anomaly` | Instant; arg `delta_ns` |
| Cross-hart inversion (when `clock_shared = false`) | (no marker; expected) | — |
| PanicMarker received | `txtrace.repair.kernel_panic` | Instant + halt; arg `panic_hart, site_name` |
| Slot magic ≠ 0x5254 (torn or stomped record) | `txtrace.repair.bad_magic` | Instant; arg `observed_magic`, `hart`, `slot_index` |
| `TxTraceRecord.version` ≠ daemon's supported version | `txtrace.repair.version_mismatch` | Instant; arg `record_version`, `daemon_version` |
| `TxPayloadTag` value not in the daemon's known table | `txtrace.repair.payload_tag_unknown` | Instant; arg `tag_value`. Distinct from `unknown_payload` (which fires when the tag is known but the daemon does not know its schema, e.g. `AgentStateChange` reserved); `payload_tag_unknown` fires when the tag value itself is unrecognized. |
| `payload_len > 16` (header lies about inline buffer size) | `txtrace.repair.payload_len_exceeded` | Instant; arg `claimed_len` |

The first nine entries are **logical** repairs (synthesized after structurally-valid decode). The last four are **framing** repairs (synthesized during decode when a slot's bytes fail validation). OBS-5 implements the four framing repairs; the logical ones land with OBS-6 (Perfetto emission) and OBS-7 (span/flow reconstruction).

OBS-HOST-V0-REPAIR-DISTINGUISH: same-hart backwards timestamps are *real* anomalies (clock bug or torn record). Cross-hart inversions without `clock_shared` are expected and unmarked. The daemon distinguishes; users see only one warning class.

OBS-HOST-V0-REPAIR-NOT-FATAL: every repair scenario produces a marker and continues. The daemon never aborts on a damaged record; it surfaces the damage and proceeds. Only the header-validation failure ([§4.1](#41-header-validation)) aborts.

## 11. Perfetto emission

<!-- txdoc:OBS-HOST-V0-PERFETTO-1 -->

MVP outputs the minimal useful Perfetto packet set:

| Perfetto packet | Sourced from | Notes |
|---|---|---|
| `TrackDescriptor` | `TrackDescriptor` records + boot hart enumeration | One per hart at boot; per task/process on creation |
| `TrackEvent { TYPE_SLICE_BEGIN }` | SpanBegin | Span timing, attached `name_iid` |
| `TrackEvent { TYPE_SLICE_END }` | SpanEnd | End timing |
| `TrackEvent { TYPE_INSTANT }` | Instant + repair markers | Point events |
| `TrackEvent.flow_ids` | YieldBegin, WaitSourceNotify | Producer/consumer flow material |
| `TrackEvent.terminating_flow_ids` | Resume | Flow closure |
| `DebugAnnotation` (on TrackEvent) | ArgContinuation | Arg key/value pairs |
| `InternedData.event_names` | TxTraceRecord.name | Interned EventNameId → friendly name |
| `InternedData.debug_annotation_names` | PayloadArgValue.key | ArgNameId → friendly name |
| `ClockSnapshot` | Trace start | Pin clock to TRACE_CLOCK_BOOT |

Deferred:
- `process_tree` (full process metadata)
- `TYPE_COUNTER` (counter events) — wait on actual counter records emitted
- Compressed `.zst` output
- Perfetto producer-SDK live mode (socket protocol to a running Perfetto)

OBS-HOST-V0-PERFETTO-PROST: protobuf encoder via `prost`. Schema cribbed from Perfetto's `protos/perfetto/trace/track_event/track_event.proto` and `track_descriptor.proto`. No need to encode the entire Perfetto schema — only the packets MVP emits.

## 12. Filtering

<!-- txdoc:OBS-HOST-V0-FILTER-1 -->

Host-side filters reduce trace size and UI noise without requiring kernel rebuild:

| Filter | CLI flag | Effect |
|---|---|---|
| Level | `--level <0..6>` | Drop records below this level. |
| Hart | `--hart <id>` | Keep only records from these harts. |
| Pid | `--pid <id>` | Keep only records whose subject pid matches. |
| Task | `--task <id>` | Keep only records on this task track. |
| Category | `--category <prefix>` | Keep only events whose name starts with prefix (after resolution). |
| Time window | `--from <ns> --to <ns>` | Keep records in [from, to]. |

OBS-HOST-V0-FILTER-PHILOSOPHY: kernel gates reduce *runtime cost*; host filters reduce *trace size*. They are independent. A kernel compiled with all levels on and filtered to L0+L2 at the daemon still pays the kernel cost.

## 13. Privacy controls

<!-- txdoc:OBS-HOST-V0-PRIVACY-1 -->

Sharing traces externally needs redaction. Modes:

| Mode | CLI flag | Behavior |
|---|---|---|
| Hash pointers | `--redact-ptr` | Replace `value_kind = Ptr` values with `siphash(boot_id, ptr) & 0xffff_ffff`. Preserves equality across the trace; loses value. |
| Strip pids | `--redact-pid` | Replace pids with stable per-trace aliases (`pid-A`, `pid-B`). |
| Hash names | `--redact-names <substr>` | For event names matching `<substr>`, replace with `name-N`. |
| Length-cap strings | `--max-arg-len <N>` | Truncate ArgValue strings to N bytes; in v0 this is a no-op since v0 doesn't emit strings. |

OBS-HOST-V0-PRIVACY-DEFAULTS: no redaction by default. Privacy is opt-in per invocation. Document the trade-offs (e.g., pointer hashing preserves "same pointer" relationships at the cost of address visibility).

## 14. Testing

<!-- txdoc:OBS-HOST-V0-TESTS-1 -->

MVP test suite:

| Test class | Coverage | Inputs |
|---|---|---|
| Golden trace decode | Decode known-good binary → known Perfetto output | `tests/golden/*.txtrace0` + matching `.pftrace.expected` |
| Fuzz decode | Reject malformed records without panic | `cargo-fuzz` on `decode_record` |
| Version compatibility | Reject unsupported header.version with clear error | Synthetic headers with version=99 |
| Span repair | Unbalanced begins, orphan ends, attach-late | Synthetic record sequences |
| Loss repair | Ring overflow synthesis, attach-late synthesis | Synthetic over-runs |
| Cross-hart merge | Multi-hart record sequences with timestamps | Synthetic |
| Flow-ID collision | SipHash output uniqueness on large input set | Property test |
| Transport (file replay) | Captured region replays identically | Capture-then-replay round trip |
| Transport (snapshot) | Post-crash region decode | Synthetic crashed region |

OBS-HOST-V0-TEST-CORPUS: maintain `tests/golden/` with representative captures: one syscall (sys_read), one yield+resume, one panic, one attach-late, one overflow. CI re-validates these.

## 15. Output modes

<!-- txdoc:OBS-HOST-V0-OUTPUT-1 -->

```text
Default: write <output>.pftrace (Perfetto protobuf)
--json: write <output>.json (human-readable debug format)
--both: write both
--stdout: write protobuf to stdout (for piping into Perfetto UI fetch)
```

OBS-HOST-V0-OUTPUT-MVP: `.pftrace` and `--json` only. Live mode (`--live` with continuous output to a running Perfetto via producer SDK) is deferred.

## 16. CLI shape

<!-- txdoc:OBS-HOST-V0-CLI-1 -->

```text
tx-trace-daemon <transport> [filters] [output] [redaction] [meta]

Transports (exactly one required):
  --live <path>           Live mmap of an ivshmem region
  --replay <file>         Replay a captured region
  --snapshot <file>       Decode a captured region once and halt
  --pipe                  Read records from stdin (test-only)

Filters (any):
  --level <0..6>
  --hart <id>             (repeatable)
  --pid <id>              (repeatable)
  --task <id>             (repeatable)
  --category <prefix>     (repeatable)
  --from <ns> --to <ns>

Output:
  -o, --output <prefix>   Output file prefix (default: trace)
  --pftrace               Emit .pftrace (default on)
  --json                  Emit .json debug dump
  --stdout                Emit protobuf to stdout

Redaction:
  --redact-ptr
  --redact-pid
  --redact-names <substr>
  --max-arg-len <N>

Meta:
  --names-dir <path>      Where to find names.json (default: ./names/)
  --boot-id <hex>         Override boot_id (for replay; auto-detected from region)
  -v, --verbose           Verbose decoding warnings
```

## 17. MVP boundary

<!-- txdoc:OBS-HOST-V0-MVP-1 -->

In scope for OBS-5 + OBS-6:
- mmap / file-replay / snapshot transports (live with `memory-backend-file`).
- Header validation + region rejection.
- Per-hart ring drain with overrun and lost-counter tracking.
- Record decode for `Nop`, `TrackDescriptor`, `SpanBegin`, `SpanEnd`, `Instant`, `ArgContinuation`.
- Payload decode for syscall, drive, step, yield, resume, wait-source-notify, arg-value.
- Span table reconstruction with repair markers.
- Flow ID computation and producer/consumer pairing.
- Name resolution via out-of-band `names.json`.
- Loss synthesis + attach-late synthesis.
- Track registry for hart and task tracks; subject process tracks from syscall enters.
- Filtering by level/hart/pid/task/category/time.
- Output to `.pftrace` + `--json`.

Deferred:
- Embedded string table (`header.string_table_off != 0`).
- Counter / Mutation record kinds (L5/L6).
- OnAgent endpoint tracks and round-trip flows.
- TrackTombstone aging.
- PanicMarker emission and halt semantics.
- Live Perfetto producer-SDK socket mode.
- Compressed output (zstd).
- Full process_tree packets.
- TrackEvent annotations beyond `DebugAnnotation` (no logs / FrameBufferFlush / etc.).

## 18. Design rules

<!-- txdoc:OBS-HOST-V0-RULES-1 -->

OBS-HOST-V0-RULE-1. **Never backpressure the guest.** The daemon may lag, must not block. Overrun is recoverable; daemon slowness is not a kernel concern.

OBS-HOST-V0-RULE-2. **Wake/flow visualization is presentation, not truth.** Perfetto flows show that a wake happened; subsystem state remains the truth source. Display flows as visualization aids, not assertions.

OBS-HOST-V0-RULE-3. **Damage markers are first-class.** Every form of damage (overflow, attach-late, missing-end, unknown kind, clock anomaly) becomes a visible Perfetto event with `txtrace.repair.*` category. Users see what's wrong.

OBS-HOST-V0-RULE-4. **No fail-silent decoding.** Unknown values produce one warning per occurrence type per session; decoder continues. Header validation failures abort with structured error.

OBS-HOST-V0-RULE-5. **Kernel-host coupling is the wire format only.** Beyond `tx-observe-types`, the daemon evolves independently. New Perfetto features added in daemon do not require kernel changes.

OBS-HOST-V0-RULE-6. **Tests are records, not assertions about Perfetto.** Golden tests compare daemon output protobuf byte-for-byte against captured-known-good `.pftrace` files. Don't assert against Perfetto's internal representation.

## 19. Open questions

<!-- txdoc:OBS-HOST-V0-OPEN-1 -->

19.1. **Reactor sched_switch track.** Visualizing "which task held which hart" requires kernel hooks at task dispatch/yield/park (deferred per [`08_OBSERVATION_v1.md §15.6`](08_OBSERVATION_v1.md)). When those land, daemon adds a `hart sched` sub-track per hart.

19.2. **Live Perfetto producer mode.** Streaming directly into a running `trace_processor_shell` via socket. Useful for very long traces. Deferred.

19.3. **Process_tree.** A full `process_tree` Perfetto packet would let viewers show parent-child process relationships. Requires kernel-side process metadata emission (parent pid, comm names) that v0 cannot provide without dynamic string interning.

19.4. **Counter aggregation.** `Counter` records emit per-sample. For high-frequency counters (mailbox occupancy at MHz), a host-side downsampling layer (e.g., 100-sample percentile bins) would be useful.

19.5. **Multiple kernel boot replay.** A single capture may span multiple `boot_id` values if the kernel rebooted. v0 daemon refuses; v0.x may stitch multiple boots into a single trace with a `kernel_reboot` instant at each transition.

## 20. Cross-references

<!-- txdoc:OBS-HOST-V0-XREFS-1 -->

- Framework: [`08_OBSERVATION_v1.md`](08_OBSERVATION_v1.md).
- Wire format: [`08_OBSERVATION_SERIALIZATION_v0.md`](08_OBSERVATION_SERIALIZATION_v0.md).
- Runtime model: [`Reactor_concept_v5_RefactorSpec v4.md`](Reactor_concept_v5_RefactorSpec%20v4.md) for `WaitGeneration` and flow material origins.
- Step model: [`03_STEP_MODEL_v2.md`](03_STEP_MODEL_v2.md) for `StepOutcome` shapes the daemon reconstructs.
- Perfetto wire format reference (external): https://perfetto.dev/docs/reference/trace-packet-proto and `protos/perfetto/trace/track_event/`.
