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
use crate::perfetto::proto::{
    Clock, ClockSnapshot, DebugAnnotation, EventName, InternedData, Trace,
    TracePacket, TrackEvent, TrackEventType, CLOCK_BOOTTIME,
    CLOCK_CUSTOM_TXTRACE_BASE, SEQ_INCREMENTAL_STATE_CLEARED, TRUSTED_SEQ_ID,
};
use crate::perfetto::span::{SpanEntry, SpanTable};
use crate::perfetto::track::TrackRegistry;

use tx_observe_types::{TxPayloadTag, TxTraceKind};

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
    fn ensure_clock_snapshot(&mut self) {
        if self.emitted_clock_snapshot {
            return;
        }
        self.emitted_clock_snapshot = true;

        // Map TxTraceClockId to a Perfetto clock_id:
        //   Unknown(0)     → custom clock (CLOCK_CUSTOM_TXTRACE_BASE + 0)
        //   RiscvTime(1)   → BOOTTIME (6) — RISC-V `time` CSR tracks boot time
        //   ArmCntvct(2)   → BOOTTIME (6)
        //   X86TscInv(3)   → custom (not directly BOOTTIME without calibration)
        //   HostNanos(4)   → BOOTTIME (6) — std monotonic ns is BOOTTIME equivalent
        let perfetto_clock_id = match self.clock_id {
            1 | 2 | 4 => CLOCK_BOOTTIME,
            _ => CLOCK_CUSTOM_TXTRACE_BASE,
        };

        // unit_multiplier_ns: 1e9 / freq_hz.  If freq unknown, emit 1 (treat as ns).
        let unit_multiplier_ns = if self.clock_freq_hz == 0 {
            1
        } else {
            (1_000_000_000u64).saturating_div(self.clock_freq_hz)
        };

        let snap = ClockSnapshot {
            clocks: vec![Clock {
                clock_id: Some(perfetto_clock_id),
                timestamp: Some(0), // reference point: ts=0 in trace = time=0
                is_incremental: Some(false),
                unit_multiplier_ns: Some(unit_multiplier_ns),
            }],
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

        match kind_byte {
            k if k == TxTraceKind::TrackDescriptor as u8 => {
                self.handle_track_descriptor(r, r.hart);
            }
            k if k == TxTraceKind::SpanBegin as u8 => {
                let span_id = parse_hex_u64(&r.span);
                let name_id = parse_hex_u32(&r.name_id);
                let (iid, new_name) = self.names.intern(name_id);
                let entry = SpanEntry { name_iid: iid, begin_ts: r.ts, track_uuid: hart_uuid };
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
                        track_uuid: Some(hart_uuid),
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
                    let (iid, new_name) = self.names.intern(name_id);
                    let interned_data = new_name.map(|n| InternedData {
                        event_names: vec![EventName { iid: Some(iid), name: Some(n) }],
                        debug_annotation_names: vec![],
                    });
                    let pkt = TracePacket {
                        timestamp: Some(r.ts),
                        timestamp_clock_id: Some(self.perfetto_clock_id()),
                        trusted_packet_sequence_id: Some(SEQ_ID),
                        track_event: Some(TrackEvent {
                            track_uuid: Some(hart_uuid),
                            r#type: Some(TrackEventType::Instant as i32),
                            name_iid: Some(iid),
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

    fn perfetto_clock_id(&self) -> u32 {
        match self.clock_id {
            1 | 2 | 4 => CLOCK_BOOTTIME,
            _ => CLOCK_CUSTOM_TXTRACE_BASE,
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
    #[allow(dead_code)]
    pub fn into_packets(mut self) -> Vec<TracePacket> {
        let remaining = self.spans.flush_all(self.last_ts);
        for cs in remaining {
            self.emit_unbalanced_begin_repair(&cs);
        }
        self.packets
    }

    /// For testing: encode to bytes without writing to disk.
    #[allow(dead_code)]
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
