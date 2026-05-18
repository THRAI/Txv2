//! PftraceWriter — accumulates Perfetto packets from a `DecodedEvent` stream
//! and writes a binary `.pftrace` file.
//!
//! Pipeline:
//!   DecodedEvent stream → PftraceWriter::push() → packet accumulator
//!                       → PftraceWriter::finish() → write Trace to file
//!
//! This implements §11 (Perfetto emission), §7 (track registry), §8 (span
//! reconstruction), §9 (flow reconstruction), §10 (repair markers), §F
//! (interned event names), §G (ClockSnapshot) from OBS-6.

use std::path::Path;
use prost::Message;

use crate::decode::{DecodedEvent, DecodedRecord, RepairRecord};
use crate::perfetto::interned::InternTable;
use std::collections::HashMap;

use crate::perfetto::proto::{
    Clock, ClockSnapshot, DebugAnnotation, DebugAnnotationName, EventName, InternedData, Trace,
    TracePacket, TrackEvent, TrackEventType, CLOCK_BOOTTIME, CLOCK_CUSTOM_TXTRACE_BASE,
    SEQ_INCREMENTAL_STATE_CLEARED, TRUSTED_SEQ_ID,
};
use crate::perfetto::span::{SpanEntry, SpanTable};
use crate::perfetto::track::TrackRegistry;

use tx_observe_types::{TxPayloadTag, TxTraceKind, TxTraceLevel};

/// The single trusted packet sequence id for this producer.
const SEQ_ID: u32 = TRUSTED_SEQ_ID;

/// Accumulates packets and maintains reconstruction state.
pub struct PftraceWriter {
    packets: Vec<TracePacket>,
    tracks: TrackRegistry,
    spans: SpanTable,
    names: InternTable,
    /// boot_id from the header (seeds flow hash keys).
    boot_id: u64,
    /// Trace clock frequency (Hz); 0 = unknown.
    clock_freq_hz: u64,
    /// Raw clock_id from the header (TxTraceClockId discriminant).
    clock_id: u32,
    /// Whether any packet has been emitted (used to track the interning epoch).
    emitted_clock_snapshot: bool,
    /// The last seen timestamp (used for stale-span flush).
    last_ts: u64,
    /// Whether the first sequence packet has been emitted (needs
    /// SEQ_INCREMENTAL_STATE_CLEARED on the first packet carrying interned data).
    first_sequence_packet: bool,
    /// OBS-9 sched-switch state: which task is currently dispatched on
    /// each hart. Updated by `SpanBegin(Sched)` / `SpanEnd(Sched)`
    /// records; consulted by every non-Sched slice emit so the slice
    /// lands on the per-task Perfetto track instead of the
    /// hart-global track. `None` means "no task currently dispatched
    /// on this hart" — slices fall back to the hart track (matches
    /// the boot-time pre-Sched behaviour).
    current_task_per_hart: HashMap<u16, u32>,
    /// Currently-dispatched process id (`PayloadSchedSwitch.process_id_low`)
    /// per hart, paired with `current_task_per_hart`. Used so the
    /// daemon can emit per-process `ProcessDescriptor` tracks that
    /// parent the per-thread (`tid-<N>`) tracks, surfacing real
    /// `pid=<N> tid=<M>` in Perfetto slice details instead of the
    /// synthetic `hart0[0] txKernel[1]` placeholders. `0` is the
    /// "no PID known" sentinel; the writer falls back to the
    /// hart-flat task track in that case.
    current_pid_per_hart: HashMap<u16, u32>,
    /// Cached `pid → comm` mapping seeded by `PayloadProcessLabel`
    /// Instants. Consulted when first materialising a process track
    /// so the `ProcessDescriptor.process_name` reflects the real PCB
    /// short name (`busybox`, `basic_exec`) instead of the synthetic
    /// `pid-<N>` fallback. Unset PIDs fall through to the synthetic
    /// name (graceful degrade for traces captured before the OBS-9
    /// §15.7 wire format landed).
    process_comm: HashMap<u32, String>,
}

impl PftraceWriter {
    /// Create a new writer.
    ///
    /// `clock_id` is the raw `TxTraceClockId` discriminant from the file header.
    /// `clock_freq_hz` is the trace clock frequency (Hz); 0 means unknown.
    /// `boot_id` is the kernel boot identifier (seeds flow hashes).
    pub fn new(clock_id: u32, clock_freq_hz: u64, boot_id: u64) -> Self {
        let mut w = Self {
            packets: Vec::new(),
            tracks: TrackRegistry::new(),
            spans: SpanTable::new(),
            names: InternTable::new(),
            boot_id,
            clock_freq_hz,
            clock_id,
            emitted_clock_snapshot: false,
            last_ts: 0,
            first_sequence_packet: true,
            current_task_per_hart: HashMap::new(),
            current_pid_per_hart: HashMap::new(),
            process_comm: HashMap::new(),
        };

        // Emit the "harts process" track descriptor first.
        let harts_desc = w.tracks.harts_process_descriptor();
        w.packets.push(TracePacket {
            trusted_packet_sequence_id: Some(SEQ_ID),
            track_descriptor: Some(harts_desc),
            ..Default::default()
        });

        w
    }

    /// Emit the opening ClockSnapshot once (§G).
    ///
    /// Perfetto's trace processor cannot resolve a sequence-scoped custom
    /// clock (id ≥ 64) to trace time unless the `ClockSnapshot` packet
    /// pairs it with a builtin clock at the same instant — otherwise the
    /// processor emits `CLOCK_SYNC_FAILURE_NO_PATH` and drops every
    /// referencing packet.  Two cases:
    ///
    /// 1. Header clock_id is RiscvTime/ArmCntvct/HostNanos/Unknown
    ///    (`unit_multiplier_ns == 1`): timestamps are already absolute
    ///    nanoseconds — equivalent to `BUILTIN_CLOCK_BOOTTIME`. Emit a
    ///    single-clock snapshot at clock 6 and tag every packet with
    ///    `timestamp_clock_id = 6`. No sync needed because the trace
    ///    processor's default trace clock IS BOOTTIME.
    /// 2. Header carries a non-trivial unit multiplier (e.g. RV64
    ///    `time` CSR ticks @ 1 GHz silicon → 1 ns/tick is still fine,
    ///    but a 1 MHz board produces 1000 ns/tick): emit a two-clock
    ///    snapshot pairing the custom clock (64) at timestamp 0 with
    ///    BOOTTIME at timestamp 0 in ns. This gives the trace processor
    ///    the sync path it needs (both clocks fire at the same instant)
    ///    and lets it scale future custom-clock timestamps by
    ///    `unit_multiplier_ns`.
    fn ensure_clock_snapshot(&mut self) {
        if self.emitted_clock_snapshot {
            return;
        }
        self.emitted_clock_snapshot = true;

        // unit_multiplier_ns: 1e9 / freq_hz.  If freq unknown, emit 1 (ns/tick).
        let unit_multiplier_ns = if self.clock_freq_hz == 0 {
            1
        } else {
            (1_000_000_000u64).saturating_div(self.clock_freq_hz)
        };

        let snap = if unit_multiplier_ns == 1 {
            // ns-domain clocks (Unknown / RiscvTime@1GHz / ArmCntvct@1GHz /
            // HostNanos): announce a single BOOTTIME clock; every packet
            // tags `timestamp_clock_id = 6` and resolves directly.
            ClockSnapshot {
                clocks: vec![Clock {
                    clock_id: Some(CLOCK_BOOTTIME),
                    timestamp: Some(0),
                    is_incremental: Some(false),
                    unit_multiplier_ns: Some(1),
                }],
            }
        } else {
            // Custom-rate clock: emit a sync pair so the processor can
            // compute trace-time from custom ticks.
            ClockSnapshot {
                clocks: vec![
                    Clock {
                        clock_id: Some(CLOCK_CUSTOM_TXTRACE_BASE),
                        timestamp: Some(0),
                        is_incremental: Some(false),
                        unit_multiplier_ns: Some(unit_multiplier_ns),
                    },
                    Clock {
                        clock_id: Some(CLOCK_BOOTTIME),
                        timestamp: Some(0),
                        is_incremental: Some(false),
                        unit_multiplier_ns: Some(1),
                    },
                ],
            }
        };

        self.packets.push(TracePacket {
            trusted_packet_sequence_id: Some(SEQ_ID),
            clock_snapshot: Some(snap),
            ..Default::default()
        });
    }

    /// Process one DecodedEvent, updating reconstruction state and accumulating packets.
    pub fn push(&mut self, event: &DecodedEvent) {
        self.ensure_clock_snapshot();

        match event {
            DecodedEvent::Record(r) => self.push_record(r),
            DecodedEvent::Repair(rep) => self.push_framing_repair(rep),
        }
    }

    fn push_record(&mut self, r: &DecodedRecord) {
        self.last_ts = self.last_ts.max(r.ts);

        // Parse kind byte.
        let kind_byte = kind_byte(r.kind);

        // Flush stale spans periodically (on every record).
        let stale = self.spans.flush_stale(r.ts);
        for cs in stale {
            self.emit_unbalanced_begin_repair(&cs);
        }

        // Ensure the hart track exists.
        let (hart_uuid, hart_desc) = self.tracks.ensure_hart(r.hart);
        if let Some(desc) = hart_desc {
            self.packets.push(TracePacket {
                trusted_packet_sequence_id: Some(SEQ_ID),
                track_descriptor: Some(desc),
                ..Default::default()
            });
        }

        // OBS-9 sched-switch state: SpanBegin(Sched) installs the
        // current task for this hart; SpanEnd(Sched) clears it. Done
        // BEFORE we choose a track for the record, so the Sched span
        // itself still lands on the hart track (which is what gives us
        // the sched_switch-equivalent timeline) while all subsequent
        // non-Sched slices route to the task track.
        if r.payload_tag == TxPayloadTag::SchedSwitch as u16 {
            if let Some((tid, pid)) = extract_sched_ids(r) {
                if kind_byte == TxTraceKind::SpanBegin as u8 {
                    self.current_task_per_hart.insert(r.hart, tid);
                    self.current_pid_per_hart.insert(r.hart, pid);
                } else if kind_byte == TxTraceKind::SpanEnd as u8 {
                    self.current_task_per_hart.remove(&r.hart);
                    self.current_pid_per_hart.remove(&r.hart);
                }
            }
        }

        // OBS-V1 §15.7: process-label Instants seed `pid → comm`.
        // Cache it now so the next non-Sched slice for this PID picks
        // up the real PCB name when it first materialises the process
        // track. We don't render the label itself as a slice — it's a
        // metadata pulse, consumed by the daemon.
        if r.payload_tag == TxPayloadTag::ProcessLabel as u16
            && kind_byte == TxTraceKind::Instant as u8
        {
            if let Some((pid, comm)) = extract_process_label(r) {
                self.process_comm.insert(pid, comm.clone());
                // If the process track was already created (e.g. with
                // `pid-<N>` or the parent's pre-execve comm), emit a
                // fresh TrackDescriptor with the same UUID so Perfetto
                // updates the track header to the new name. Skipped
                // when no track exists yet — first slice for this PID
                // will pick the name up from `process_comm` at
                // `ensure_thread_track_under_process` time.
                if let Some(desc) = self.tracks.rename_process_track(pid, &comm) {
                    self.packets.push(TracePacket {
                        trusted_packet_sequence_id: Some(SEQ_ID),
                        track_descriptor: Some(desc),
                        ..Default::default()
                    });
                }
            }
            // Skip emitting a Perfetto packet for the bare label
            // itself — its only job is to seed the cache and
            // (optionally) rename the existing track above.
            return;
        }

        // Choose the slice track: the per-thread track parented under
        // a per-process track when both PID and TID are known for the
        // current hart; the hart-flat task track when only TID is
        // known (older traces, kernel actors with pid=0); the hart
        // track otherwise. Sched-level records intentionally stay on
        // the hart track so the sched timeline reads "which task
        // held this CPU" at a glance — like the Linux sched_switch
        // view in Perfetto.
        let slice_track_uuid = if r.level == "Sched" {
            hart_uuid
        } else if let Some(&tid) = self.current_task_per_hart.get(&r.hart) {
            let pid = self.current_pid_per_hart.get(&r.hart).copied().unwrap_or(0);
            if pid != 0 {
                // Use the cached PCB `comm` as the process track name
                // when we've seen a ProcessLabel for this PID; fall
                // through to the synthetic `pid-<N>` otherwise.
                let proc_name = self.process_comm.get(&pid).cloned();
                let (uuid, proc_desc, thread_desc) = self
                    .tracks
                    .ensure_thread_track_under_process(pid, r.hart, tid, proc_name.as_deref());
                if let Some(d) = proc_desc {
                    self.packets.push(TracePacket {
                        trusted_packet_sequence_id: Some(SEQ_ID),
                        track_descriptor: Some(d),
                        ..Default::default()
                    });
                }
                if let Some(d) = thread_desc {
                    self.packets.push(TracePacket {
                        trusted_packet_sequence_id: Some(SEQ_ID),
                        track_descriptor: Some(d),
                        ..Default::default()
                    });
                }
                uuid
            } else {
                // No PID known — keep the legacy hart-flat task track
                // so the slice still routes off the hart-global track.
                let task_track_name = format!("task.{tid}");
                let (uuid, desc) = self.tracks.ensure_kernel_track(
                    ((r.hart as u64) << 32) | tid as u64,
                    task_track_name,
                    /* track_kind = Thread (1) */ 1,
                    Some(r.hart),
                );
                if let Some(d) = desc {
                    self.packets.push(TracePacket {
                        trusted_packet_sequence_id: Some(SEQ_ID),
                        track_descriptor: Some(d),
                        ..Default::default()
                    });
                }
                uuid
            }
        } else {
            hart_uuid
        };

        match kind_byte {
            k if k == TxTraceKind::TrackDescriptor as u8 => {
                self.handle_track_descriptor(r, r.hart);
            }
            k if k == TxTraceKind::SpanBegin as u8 => {
                let span_id = parse_hex_u64(&r.span);
                let name_id = parse_hex_u32(&r.name_id);
                let (iid, new_name) = self.names.intern(name_id);
                let entry = SpanEntry { name_iid: iid, begin_ts: r.ts, track_uuid: slice_track_uuid };
                self.spans.begin(r.hart, span_id, entry);
                // Emit the begin event; we'll emit the end when SpanEnd arrives.
                // Per Perfetto convention we emit TYPE_SLICE_BEGIN now and TYPE_SLICE_END on End.
                let interned_data = new_name.map(|n| InternedData {
                    event_names: vec![EventName { iid: Some(iid), name: Some(n) }],
                    debug_annotation_names: vec![],
                });
                let mut pkt = TracePacket {
                    timestamp: Some(r.ts),
                    timestamp_clock_id: Some(self.perfetto_clock_id()),
                    trusted_packet_sequence_id: Some(SEQ_ID),
                    track_event: Some(TrackEvent {
                        track_uuid: Some(slice_track_uuid),
                        r#type: Some(TrackEventType::SliceBegin as i32),
                        name_iid: Some(iid),
                        ..Default::default()
                    }),
                    interned_data,
                    ..Default::default()
                };
                if self.first_sequence_packet {
                    pkt.sequence_flags = Some(SEQ_INCREMENTAL_STATE_CLEARED);
                    self.first_sequence_packet = false;
                }
                self.packets.push(pkt);
            }
            k if k == TxTraceKind::SpanEnd as u8 => {
                let span_id = parse_hex_u64(&r.span);
                match self.spans.end(r.hart, span_id, r.ts, hart_uuid) {
                    Ok(cs) => {
                        // Emit TYPE_SLICE_END at end_ts on the same track.
                        let pkt = TracePacket {
                            timestamp: Some(cs.end_ts),
                            timestamp_clock_id: Some(self.perfetto_clock_id()),
                            trusted_packet_sequence_id: Some(SEQ_ID),
                            track_event: Some(TrackEvent {
                                track_uuid: Some(cs.track_uuid),
                                r#type: Some(TrackEventType::SliceEnd as i32),
                                ..Default::default()
                            }),
                            ..Default::default()
                        };
                        self.packets.push(pkt);
                    }
                    Err(orphan) => {
                        // Emit orphan_end repair instant.
                        let pkt = TracePacket {
                            timestamp: Some(orphan.ts),
                            timestamp_clock_id: Some(self.perfetto_clock_id()),
                            trusted_packet_sequence_id: Some(SEQ_ID),
                            track_event: Some(TrackEvent {
                                track_uuid: Some(orphan.track_uuid),
                                r#type: Some(TrackEventType::Instant as i32),
                                name: Some("txtrace.repair.orphan_end".to_string()),
                                categories: vec!["txtrace.repair.orphan_end".to_string()],
                                debug_annotations: vec![DebugAnnotation {
                                    name: Some("orphan_span_id".to_string()),
                                    uint_value: Some(orphan.span_id),
                                    ..Default::default()
                                }],
                                ..Default::default()
                            }),
                            ..Default::default()
                        };
                        self.packets.push(pkt);
                    }
                }
            }
            k if k == TxTraceKind::Instant as u8 => {
                // Route flow-reconstruction instants through dedicated handlers.
                // Other instants fall through to the generic interned-name path.
                if r.payload_tag == TxPayloadTag::WaitSourceNotify as u16 {
                    let (task_id_low, wait_gen) = extract_wait_source_notify_fields(r);
                    self.push_wait_source_notify(r.ts, hart_uuid, task_id_low, wait_gen);
                } else if r.payload_tag == TxPayloadTag::Resume as u16 {
                    let (task_id_low, wait_gen) = extract_resume_fields(r);
                    self.push_resume(r.ts, hart_uuid, task_id_low, wait_gen);
                } else {
                    let name_id = parse_hex_u32(&r.name_id);

                    // OBS-V1 §8.6: `ArgValue` continuation records carry the
                    // raw syscall arg (or other annotation) as the
                    // payload's `value0` field. We surface that value in
                    // TWO complementary ways:
                    //   1. As the `TrackEvent.name` itself — formatted as
                    //      e.g. `a0=0x1234`. Perfetto renders Instants as
                    //      vertical markers labeled by the event name, so
                    //      this puts the register value directly on the
                    //      timeline next to the syscall slice (no click
                    //      needed). We intern the formatted string fresh
                    //      per (arg_name, value) pair via `intern_str`.
                    //   2. As a `DebugAnnotation` named "value" — keeps
                    //      the structured value reachable from the
                    //      "Current Selection" panel so tooling can
                    //      diff/filter on it without parsing the label.
                    let arg_value =
                        (r.payload_tag == TxPayloadTag::ArgValue as u16)
                            .then(|| extract_arg_value_field(r));

                    let (iid, new_name) = if let Some(v) = arg_value {
                        // Resolve the arg's base name (`a0`, `a1`, …)
                        // through the regular names.json path, then
                        // format `name=0x<hex>` and intern THAT as the
                        // Instant's visible label.
                        let base = self
                            .names
                            .get_resolved_name(name_id)
                            .unwrap_or_else(|| format!("arg_0x{name_id:08x}"));
                        let label = format!("{base}=0x{v:x}");
                        self.names.intern_str(&label)
                    } else {
                        self.names.intern(name_id)
                    };

                    let mut debug_annotation_names = vec![];
                    let mut debug_annotations = vec![];
                    if let Some(v) = arg_value {
                        let (value_iid, value_new_name) = self.names.intern_str("value");
                        if let Some(n) = value_new_name {
                            debug_annotation_names.push(DebugAnnotationName {
                                iid: Some(value_iid),
                                name: Some(n),
                            });
                        }
                        debug_annotations.push(DebugAnnotation {
                            name_iid: Some(value_iid),
                            uint_value: Some(v),
                            ..Default::default()
                        });
                    }

                    let interned_data = if new_name.is_some()
                        || !debug_annotation_names.is_empty()
                    {
                        Some(InternedData {
                            event_names: new_name
                                .map(|n| {
                                    vec![EventName { iid: Some(iid), name: Some(n) }]
                                })
                                .unwrap_or_default(),
                            debug_annotation_names,
                        })
                    } else {
                        None
                    };

                    let pkt = TracePacket {
                        timestamp: Some(r.ts),
                        timestamp_clock_id: Some(self.perfetto_clock_id()),
                        trusted_packet_sequence_id: Some(SEQ_ID),
                        track_event: Some(TrackEvent {
                            track_uuid: Some(slice_track_uuid),
                            r#type: Some(TrackEventType::Instant as i32),
                            name_iid: Some(iid),
                            debug_annotations,
                            ..Default::default()
                        }),
                        interned_data,
                        ..Default::default()
                    };
                    self.packets.push(pkt);
                }
            }
            // Nop, ClockSnapshot, StringDescriptor, Counter, TrackTombstone,
            // PanicMarker, ArgContinuation — ignore or no Perfetto packet.
            _ => {}
        }

        // Logical repair: unknown kind.
        if r.kind == "Unknown" {
            // We already consumed the record; emit unknown_kind instant.
            let pkt = self.make_repair_instant(
                r.ts,
                hart_uuid,
                "txtrace.repair.unknown_kind",
                vec![DebugAnnotation {
                    name: Some("kind_value".to_string()),
                    uint_value: Some(0), // kind byte is not preserved in DecodedRecord for Unknown
                    ..Default::default()
                }],
            );
            self.packets.push(pkt);
        }
    }

    /// Handle a WaitSourceNotify record — compute flow_id and attach it.
    ///
    /// Called from push_record when the record's payload tag is `WaitSourceNotify`.
    pub fn push_wait_source_notify(
        &mut self,
        ts: u64,
        track_uuid: u64,
        task_id_low: u32,
        wait_gen: u64,
    ) {
        use crate::perfetto::flow::{compute_flow_id, FlowKind};
        let fid = compute_flow_id(task_id_low, wait_gen, FlowKind::SourceWake, self.boot_id);
        let pkt = TracePacket {
            timestamp: Some(ts),
            timestamp_clock_id: Some(self.perfetto_clock_id()),
            trusted_packet_sequence_id: Some(SEQ_ID),
            track_event: Some(TrackEvent {
                track_uuid: Some(track_uuid),
                r#type: Some(TrackEventType::Instant as i32),
                name: Some("WaitSourceNotify".to_string()),
                flow_ids: vec![fid],
                ..Default::default()
            }),
            ..Default::default()
        };
        self.packets.push(pkt);
    }

    /// Handle a Resume record — close the terminating flow.
    ///
    /// Called from push_record when the record's payload tag is `Resume`.
    pub fn push_resume(
        &mut self,
        ts: u64,
        track_uuid: u64,
        task_id_low: u32,
        wait_gen: u64,
    ) {
        use crate::perfetto::flow::{compute_flow_id, FlowKind};
        let fid = compute_flow_id(task_id_low, wait_gen, FlowKind::SourceWake, self.boot_id);
        let pkt = TracePacket {
            timestamp: Some(ts),
            timestamp_clock_id: Some(self.perfetto_clock_id()),
            trusted_packet_sequence_id: Some(SEQ_ID),
            track_event: Some(TrackEvent {
                track_uuid: Some(track_uuid),
                r#type: Some(TrackEventType::Instant as i32),
                name: Some("Resume".to_string()),
                terminating_flow_ids: vec![fid],
                ..Default::default()
            }),
            ..Default::default()
        };
        self.packets.push(pkt);
    }

    fn handle_track_descriptor(&mut self, r: &DecodedRecord, hart: u16) {
        // Parse payload.track_id and track_kind from the JSON payload.
        let (track_id, track_kind, name_id) =
            if let Some(p) = &r.payload {
                let track_id = p["track_id"].as_u64().unwrap_or(0);
                let track_kind = p["track_kind"].as_u64().unwrap_or(0) as u8;
                let name_id = p["name"].as_u64().unwrap_or(0) as u32;
                (track_id, track_kind, name_id)
            } else {
                return;
            };

        let name_str = {
            let (_, new_name) = self.names.intern(name_id);
            // Use the resolved name or the hex fallback already stored in InternTable.
            new_name.unwrap_or_else(|| {
                self.names
                    .get_iid(name_id)
                    .map(|_| format!("name_0x{name_id:08x}"))
                    .unwrap_or_else(|| format!("name_0x{name_id:08x}"))
            })
        };

        let (_, maybe_desc) =
            self.tracks.ensure_kernel_track(track_id, name_str, track_kind, Some(hart));
        if let Some(desc) = maybe_desc {
            self.packets.push(TracePacket {
                trusted_packet_sequence_id: Some(SEQ_ID),
                track_descriptor: Some(desc),
                ..Default::default()
            });
        }
    }

    fn push_framing_repair(&mut self, rep: &RepairRecord) {
        // Ensure hart track.
        let (hart_uuid, hart_desc) = self.tracks.ensure_hart(rep.hart);
        if let Some(desc) = hart_desc {
            self.packets.push(TracePacket {
                trusted_packet_sequence_id: Some(SEQ_ID),
                track_descriptor: Some(desc),
                ..Default::default()
            });
        }

        let pkt = self.make_repair_instant(
            self.last_ts,
            hart_uuid,
            rep.category,
            vec![DebugAnnotation {
                name: Some("details".to_string()),
                string_value: Some(rep.details.clone()),
                ..Default::default()
            }],
        );
        self.packets.push(pkt);
    }

    fn emit_unbalanced_begin_repair(&mut self, cs: &crate::perfetto::span::ClosedSpan) {
        let pkt = TracePacket {
            timestamp: Some(cs.end_ts),
            timestamp_clock_id: Some(self.perfetto_clock_id()),
            trusted_packet_sequence_id: Some(SEQ_ID),
            track_event: Some(TrackEvent {
                track_uuid: Some(cs.track_uuid),
                r#type: Some(TrackEventType::Instant as i32),
                name: Some("txtrace.repair.unbalanced_begin".to_string()),
                categories: vec!["txtrace.repair.unbalanced_begin".to_string()],
                debug_annotations: vec![DebugAnnotation {
                    name: Some("original_begin_ts".to_string()),
                    uint_value: Some(cs.begin_ts),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        self.packets.push(pkt);
    }

    fn make_repair_instant(
        &self,
        ts: u64,
        track_uuid: u64,
        category: &str,
        annotations: Vec<DebugAnnotation>,
    ) -> TracePacket {
        TracePacket {
            timestamp: Some(ts),
            timestamp_clock_id: Some(self.perfetto_clock_id()),
            trusted_packet_sequence_id: Some(SEQ_ID),
            track_event: Some(TrackEvent {
                track_uuid: Some(track_uuid),
                r#type: Some(TrackEventType::Instant as i32),
                name: Some(category.to_string()),
                categories: vec![category.to_string()],
                debug_annotations: annotations,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The clock_id tagged on every record's `timestamp_clock_id` field.
    ///
    /// Must match the snapshot emitted by `ensure_clock_snapshot`:
    /// - `unit_multiplier_ns == 1` ⇒ `CLOCK_BOOTTIME` (single-clock
    ///   snapshot path).
    /// - Otherwise the custom clock id, sync'd to BOOTTIME at trace
    ///   start by the two-clock snapshot pair.
    fn perfetto_clock_id(&self) -> u32 {
        let unit_multiplier_ns = if self.clock_freq_hz == 0 {
            1
        } else {
            (1_000_000_000u64).saturating_div(self.clock_freq_hz)
        };
        if unit_multiplier_ns == 1 {
            CLOCK_BOOTTIME
        } else {
            CLOCK_CUSTOM_TXTRACE_BASE
        }
    }

    /// Load an external name table (from names.json) into the intern table.
    pub fn load_names(&mut self, map: std::collections::HashMap<u32, String>) {
        self.names.load_external(map);
    }

    /// Finalize: flush remaining open spans and write the trace to `path`.
    pub fn finish(mut self, path: &Path) -> std::io::Result<()> {
        // Flush all remaining open spans as unbalanced_begin repairs.
        let remaining = self.spans.flush_all(self.last_ts);
        for cs in remaining {
            self.emit_unbalanced_begin_repair(&cs);
        }

        let trace = Trace { packet: self.packets };
        let mut buf = Vec::with_capacity(trace.encoded_len());
        trace.encode(&mut buf).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("prost encode: {e}"))
        })?;
        std::fs::write(path, &buf)
    }

    /// For testing: return the accumulated packets without writing to disk.
    #[cfg(test)]
    pub fn into_packets(mut self) -> Vec<TracePacket> {
        let remaining = self.spans.flush_all(self.last_ts);
        for cs in remaining {
            self.emit_unbalanced_begin_repair(&cs);
        }
        self.packets
    }

    /// For testing: encode to bytes without writing to disk.
    #[cfg(test)]
    pub fn encode_to_vec(self) -> Result<Vec<u8>, prost::EncodeError> {
        let packets = self.into_packets();
        let trace = Trace { packet: packets };
        let mut buf = Vec::with_capacity(trace.encoded_len());
        trace.encode(&mut buf)?;
        Ok(buf)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn kind_byte(kind_str: &str) -> u8 {
    match kind_str {
        "Nop"             => TxTraceKind::Nop as u8,
        "ClockSnapshot"   => TxTraceKind::ClockSnapshot as u8,
        "TrackDescriptor" => TxTraceKind::TrackDescriptor as u8,
        "StringDescriptor"=> TxTraceKind::StringDescriptor as u8,
        "SpanBegin"       => TxTraceKind::SpanBegin as u8,
        "SpanEnd"         => TxTraceKind::SpanEnd as u8,
        "Instant"         => TxTraceKind::Instant as u8,
        "Counter"         => TxTraceKind::Counter as u8,
        "TrackTombstone"  => TxTraceKind::TrackTombstone as u8,
        "PanicMarker"     => TxTraceKind::PanicMarker as u8,
        "ArgContinuation" => TxTraceKind::ArgContinuation as u8,
        _                 => 0xFF, // unknown
    }
}

fn parse_hex_u64(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

fn parse_hex_u32(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

/// Extract `(task_id_low, wait_generation)` from a decoded `WaitSourceNotify`
/// payload JSON for flow-id computation.
///
/// Returns `(0, 0)` if the payload is absent or malformed (safe fallback —
/// the flow hash will still be computed, just with degenerate inputs).
fn extract_wait_source_notify_fields(r: &DecodedRecord) -> (u32, u64) {
    if let Some(p) = &r.payload {
        let task_id_low = p["task_id_low"].as_u64().unwrap_or(0) as u32;
        let wait_gen_low = p["wait_generation_low"].as_u64().unwrap_or(0) as u32;
        (task_id_low, wait_gen_low as u64)
    } else {
        (0, 0)
    }
}

/// Extract `(task_id_low, wait_generation)` from a decoded `Resume` payload
/// JSON for flow-id computation.
///
/// Returns `(0, 0)` if the payload is absent or malformed.
fn extract_resume_fields(r: &DecodedRecord) -> (u32, u64) {
    if let Some(p) = &r.payload {
        let wait_gen = p["wait_generation"].as_u64().unwrap_or(0);
        // Resume does not carry task_id_low directly (OBS-3b deferral);
        // use 0 until task_id threading lands.
        (0u32, wait_gen)
    } else {
        (0, 0)
    }
}

/// Extract `value0: u64` from a decoded `ArgValue` payload JSON.
///
/// Each syscall arg (and other annotation continuation) carries its
/// numeric u64 in this field. Returns `0` when the payload is missing
/// or malformed — safe fallback that surfaces as a `value=0` annotation
/// rather than dropping the chip.
fn extract_arg_value_field(r: &DecodedRecord) -> u64 {
    r.payload
        .as_ref()
        .and_then(|p| p["value0"].as_u64())
        .unwrap_or(0)
}

/// Extract `(process_id_low, comm_string)` from a `ProcessLabel`
/// payload JSON. `comm` is rendered from the wire `[u8; 12]` by
/// stopping at the first NUL or non-printable byte; if no printable
/// bytes precede the NUL we drop the label (graceful degrade so a
/// corrupt name doesn't replace a synthetic `pid-<N>` with the empty
/// string).
fn extract_process_label(r: &DecodedRecord) -> Option<(u32, String)> {
    let p = r.payload.as_ref()?;
    let pid = p["process_id_low"].as_u64()? as u32;
    let bytes = p["comm"].as_array()?;
    let mut s = String::new();
    for b in bytes {
        let byte = b.as_u64()? as u8;
        if byte == 0 {
            break;
        }
        if !(0x20..=0x7e).contains(&byte) {
            return None;
        }
        s.push(byte as char);
    }
    if s.is_empty() {
        return None;
    }
    Some((pid, s))
}

/// Extract `(task_id_low, process_id_low)` from a `SchedSwitch` payload JSON.
///
/// Used by the writer's sched-switch state machine to keep
/// `current_task_per_hart` and `current_pid_per_hart` in sync with
/// the kernel's per-hart dispatch decisions. `process_id_low`
/// defaults to `0` for older traces that pre-date the PID-plumbing
/// rev — those slices still route to the hart-flat task track via
/// the `pid == 0` fallback path. Returns `None` only when the
/// payload is missing or `task_id_low` is unreadable; the caller
/// leaves slots unchanged in that case.
fn extract_sched_ids(r: &DecodedRecord) -> Option<(u32, u32)> {
    let p = r.payload.as_ref()?;
    let tid = p["task_id_low"].as_u64()? as u32;
    let pid = p["process_id_low"].as_u64().unwrap_or(0) as u32;
    Some((tid, pid))
}
