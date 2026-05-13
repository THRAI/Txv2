//! File-replay transport for OBS-5.
//!
//! Reads a captured `.txtrace` region file (raw `TxTraceHeader` followed by
//! N per-hart `TxTraceHartRing` headers followed by N sets of ring slot arrays)
//! and feeds each slot through the decoder.
//!
//! # File layout
//!
//! ```text
//! [TxTraceHeader          : 72 bytes]
//! [TxTraceHartRing[0]     : 208 bytes] ─┐
//! [TxTraceHartRing[1]     : 208 bytes]  │  hart ring headers
//! ...                                    │
//! [TxTraceHartRing[N-1]   : 208 bytes] ─┘
//! [TxTraceRecord; 1<<ring_order]  hart 0 slots
//! [TxTraceRecord; 1<<ring_order]  hart 1 slots
//! ...
//! ```
//!
//! The exact offset of the ring headers is given by `header.rings_off`.
//! The slot arrays follow each ring header immediately (since
//! `TxTraceHartRing` is followed in the region by its slots).
//!
//! # Producer/consumer pointers
//!
//! In a live mmap the daemon would use Acquire-load atomics.  In file-replay
//! we std::fs::read the whole file and treat the atomic fields as plain u64
//! LE values — the file was written from a quiesced or captured region so
//! there is no concurrent producer.  We read `producer` and `consumer` from
//! the raw bytes at their spec'd offsets in `TxTraceHartRing`.

use std::collections::HashMap;
use std::mem::size_of;
use std::path::Path;

use tx_observe_types::{TxTraceHeader, TxTraceHartRing, TxTraceRecord};
use tx_observe_types::header::TX_TRACE_MAGIC;

use crate::decode::{DecodedEvent, decode_slot};
use crate::emit_json;
use crate::perfetto::writer::PftraceWriter;

/// Maximum hart count the daemon will accept (§4.1 of the host doc).
const MAX_HARTS_DAEMON: u16 = 256;

/// Supported header version.
const SUPPORTED_HEADER_VERSION: u16 = 0;

/// Supported record size (bytes).
const SUPPORTED_RECORD_SIZE: u16 = 80;

/// Offsets into the raw `TxTraceHartRing` bytes for the atomic fields.
/// Derived from the struct layout documented in `TxTraceHartRing`'s doc
/// comment and `08_OBSERVATION_SERIALIZATION_v0.md §4`:
///   offset  64: producer: AtomicU64  (8 bytes)
///   offset 128: consumer: AtomicU64  (8 bytes)
const RING_PRODUCER_OFF: usize = 64;
const RING_CONSUMER_OFF: usize = 128;

/// Run the file-replay transport.
///
/// Opens `path`, validates the header, then iterates over all hart rings and
/// decodes every filled slot.  Each decoded event is emitted as a
/// newline-delimited JSON record.
///
/// If `min_level` is `Some(n)`, records whose `level` byte is < n are dropped.
pub fn run(path: &Path, min_level: Option<u8>) -> std::io::Result<()> {
    let data = std::fs::read(path)?;
    let events = decode_file_bytes(&data).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;

    for event in &events {
        // Apply level filter if requested.
        if let Some(min) = min_level {
            if let DecodedEvent::Record(r) = event {
                let level_byte = level_str_to_byte(r.level);
                if level_byte < min {
                    continue;
                }
            }
        }
        emit_json::emit(event)?;
    }
    Ok(())
}

/// Run the file-replay transport in Perfetto pftrace output mode (OBS-6).
///
/// Decodes the trace region, feeds every `DecodedEvent` through `PftraceWriter`,
/// and writes the resulting `.pftrace` binary to `out_path`.
///
/// `names_map` optionally maps EventNameId → human name (loaded from names.json).
pub fn run_pftrace(
    path: &Path,
    out_path: &Path,
    names_map: Option<HashMap<u32, String>>,
) -> std::io::Result<()> {
    let data = std::fs::read(path)?;

    // Read header fields needed for the writer (clock_id, clock_freq_hz, boot_id).
    let (clock_id, clock_freq_hz, boot_id) = read_header_meta(&data).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;

    let events = decode_file_bytes(&data).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;

    let mut writer = PftraceWriter::new(clock_id, clock_freq_hz, boot_id);

    if let Some(map) = names_map {
        // Load external names before processing events so intern() resolves them.
        // PftraceWriter exposes the intern table via a dedicated loader.
        writer.load_names(map);
    }

    for event in &events {
        writer.push(event);
    }

    writer.finish(out_path)
}

/// Extract `(clock_id, clock_freq_hz, boot_id)` from the raw header bytes.
///
/// Returns an error string if the header is too small or magic is invalid
/// (the full validation happens inside `decode_file_bytes`; here we just
/// need the three fields for the Perfetto writer).
fn read_header_meta(data: &[u8]) -> Result<(u32, u64, u64), String> {
    if data.len() < size_of::<TxTraceHeader>() {
        return Err("file too small for header".to_string());
    }
    // Safety: TxTraceHeader is POD; we checked length above.
    let hdr: TxTraceHeader =
        unsafe { std::ptr::read_unaligned(data.as_ptr() as *const TxTraceHeader) };
    Ok((hdr.clock_id, hdr.clock_freq_hz, hdr.boot_id))
}

/// Decode all records from a raw region byte slice.
///
/// Returns a `Vec<DecodedEvent>` on success, or a human-readable error string
/// if the header is invalid.
///
/// This function is also the library entry point used by the integration test.
pub fn decode_file_bytes(data: &[u8]) -> Result<Vec<DecodedEvent>, String> {
    // ── Header validation ─────────────────────────────────────────────────────
    if data.len() < size_of::<TxTraceHeader>() {
        return Err(format!(
            "file too small ({} bytes) for TxTraceHeader ({} bytes)",
            data.len(),
            size_of::<TxTraceHeader>()
        ));
    }

    // Safety: we checked the slice is long enough; TxTraceHeader is POD.
    let header: TxTraceHeader =
        unsafe { std::ptr::read_unaligned(data.as_ptr() as *const TxTraceHeader) };

    if header.magic != TX_TRACE_MAGIC {
        return Err(format!(
            "bad header magic: expected 0x{TX_TRACE_MAGIC:08x}, got 0x{:08x}",
            header.magic
        ));
    }
    if header.version > SUPPORTED_HEADER_VERSION {
        return Err(format!(
            "unsupported header version {}: only version {} is supported",
            header.version, SUPPORTED_HEADER_VERSION
        ));
    }
    if header.record_size != SUPPORTED_RECORD_SIZE {
        return Err(format!(
            "unsupported record_size {}: expected {}",
            header.record_size, SUPPORTED_RECORD_SIZE
        ));
    }
    if header.hart_count > MAX_HARTS_DAEMON {
        return Err(format!(
            "hart_count {} exceeds daemon maximum {}",
            header.hart_count, MAX_HARTS_DAEMON
        ));
    }
    if header.ring_order < 2 || header.ring_order > 24 {
        return Err(format!(
            "ring_order {} is outside the valid range [2, 24]",
            header.ring_order
        ));
    }
    let rings_off = header.rings_off as usize;
    if rings_off < size_of::<TxTraceHeader>() {
        return Err(format!(
            "rings_off {} is before the end of the header ({})",
            rings_off,
            size_of::<TxTraceHeader>()
        ));
    }

    let hart_count = header.hart_count as usize;
    let ring_order = header.ring_order as u32;
    let slot_count = 1usize << ring_order;
    let record_size = size_of::<TxTraceRecord>();
    let ring_header_size = size_of::<TxTraceHartRing>();
    let ring_data_size = ring_header_size + slot_count * record_size;

    // Verify region is large enough for all ring headers + slots.
    let required_size = rings_off + hart_count * ring_data_size;
    if data.len() < required_size {
        return Err(format!(
            "file too small: need at least {required_size} bytes for {} harts \
             (rings_off={rings_off}, ring_data_size={ring_data_size}), got {}",
            hart_count,
            data.len()
        ));
    }

    // ── Ring drain ────────────────────────────────────────────────────────────
    let mut events: Vec<DecodedEvent> = Vec::new();

    for h in 0..hart_count {
        let ring_base = rings_off + h * ring_data_size;
        let ring_bytes = &data[ring_base..ring_base + ring_header_size];

        // Read producer and consumer counters from their raw offsets inside the
        // ring header.  In file-replay mode these are plain little-endian u64
        // values (no concurrency).
        let producer = read_u64_le(ring_bytes, RING_PRODUCER_OFF);
        let consumer = read_u64_le(ring_bytes, RING_CONSUMER_OFF);

        // Handle overrun: if producer outpaced the consumer by more than
        // slot_count, skip overwritten slots.
        let effective_consumer = if producer.wrapping_sub(consumer) > slot_count as u64 {
            producer.wrapping_sub(slot_count as u64)
        } else {
            consumer
        };

        let slots_base = ring_base + ring_header_size;
        let mut c = effective_consumer;
        while c != producer {
            let slot_idx = (c & (slot_count as u64 - 1)) as usize;
            let slot_off = slots_base + slot_idx * record_size;
            let slot_bytes = &data[slot_off..slot_off + record_size];
            let event = decode_slot(h as u16, slot_bytes);
            events.push(event);
            c = c.wrapping_add(1);
        }
    }

    Ok(events)
}

/// Read a little-endian `u64` from `buf` at `offset`.
fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    let bytes: [u8; 8] = buf[offset..offset + 8].try_into().expect("u64 read");
    u64::from_le_bytes(bytes)
}

/// Convert a level name string back to its numeric byte (for filtering).
fn level_str_to_byte(level: &str) -> u8 {
    match level {
        "Boundary" => 0,
        "Script"   => 1,
        "Drive"    => 2,
        "Yield"    => 3,
        "Step"     => 4,
        "Phase"    => 5,
        "Mutation" => 6,
        _          => 0,
    }
}

// ---------------------------------------------------------------------------
// Integration test: synthetic .txtrace roundtrip
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;
    use tx_observe_types::{
        TxTraceHeader, TxTraceKind, TxTraceLevel, TxTraceRecord,
        payload::TxPayloadTag,
        header::TX_TRACE_MAGIC,
    };

    const RECORD_MAGIC: u16 = 0x5254;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn write_u64_le(buf: &mut [u8], offset: usize, v: u64) {
        buf[offset..offset + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn write_u32_le(buf: &mut [u8], offset: usize, v: u32) {
        buf[offset..offset + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn write_u16_le(buf: &mut [u8], offset: usize, v: u16) {
        buf[offset..offset + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// Build a raw TxTraceRecord byte buffer.
    fn make_record_bytes(
        magic: u16,
        version: u8,
        kind: u8,
        level: u8,
        hart: u16,
        seq: u64,
        ts: u64,
        span: u64,
        parent: u64,
        name: u32,
        payload_tag: u16,
        payload_len: u16,
        payload_bytes: [u8; 16],
    ) -> Vec<u8> {
        let mut buf = vec![0u8; size_of::<TxTraceRecord>()];
        write_u16_le(&mut buf, 0, magic);     // magic
        buf[2] = version;                       // version
        buf[3] = kind;                          // kind
        buf[4] = level;                         // level
        // flags=0, arg_count=0, _pad0=0 at bytes 5,6,7
        write_u16_le(&mut buf, 8, hart);        // hart
        // _pad1, _pad2 are zero
        write_u64_le(&mut buf, 16, seq);        // seq
        write_u64_le(&mut buf, 24, ts);         // ts
        write_u64_le(&mut buf, 32, span);       // span
        write_u64_le(&mut buf, 40, parent);     // parent
        write_u32_le(&mut buf, 48, name);       // name
        write_u16_le(&mut buf, 52, payload_tag);// payload_tag
        write_u16_le(&mut buf, 54, payload_len);// payload_len
        buf[56..72].copy_from_slice(&payload_bytes); // payload
        // _pad3 is zero (bytes 72-79)
        buf
    }

    /// Build a synthetic one-hart .txtrace file with the given record bytes.
    ///
    /// Layout:
    ///   [TxTraceHeader : 72 bytes]
    ///   [TxTraceHartRing : 208 bytes]  <- producer = num_records, consumer = 0
    ///   [TxTraceRecord; slot_count]    <- first num_records slots filled
    fn make_trace_file(records: &[Vec<u8>]) -> Vec<u8> {
        let ring_order: u8 = 4; // 16 slots — more than enough
        let slot_count = 1usize << ring_order;
        let record_size = size_of::<TxTraceRecord>(); // 80
        let ring_header_size = size_of::<TxTraceHartRing>(); // 208
        let rings_off = size_of::<TxTraceHeader>(); // 72

        let total = rings_off + ring_header_size + slot_count * record_size;
        let mut buf = vec![0u8; total];

        // ── TxTraceHeader (72 bytes) ──────────────────────────────────────
        write_u32_le(&mut buf, 0, TX_TRACE_MAGIC);          // magic
        write_u16_le(&mut buf, 4, 0);                        // version = 0
        write_u16_le(&mut buf, 6, rings_off as u16);         // header_len
        buf[8] = 1;                                           // endian = LE
        buf[9] = 8;                                           // ptr_width
        write_u16_le(&mut buf, 10, 80);                      // record_size
        write_u16_le(&mut buf, 12, 1);                       // hart_count = 1
        buf[14] = ring_order;                                 // ring_order
        // flags = 0, _pad0 = 0 (bytes 15, 16-19)
        // boot_id at 24 — leave 0
        // clock_id at 32 — leave 0 (Unknown)
        // clock_freq_hz at 40 — leave 0
        // string_table_off at 48, string_table_len at 56 — leave 0
        write_u64_le(&mut buf, 64, rings_off as u64);        // rings_off

        // ── TxTraceHartRing (208 bytes) ───────────────────────────────────
        let ring_base = rings_off;
        // hart_id = 0, flags = 0 (bytes 0-3 of ring)
        // producer at ring_base + RING_PRODUCER_OFF (= 64):
        write_u64_le(&mut buf, ring_base + RING_PRODUCER_OFF, records.len() as u64);
        // consumer at ring_base + RING_CONSUMER_OFF (= 128): 0

        // ── Slot data ─────────────────────────────────────────────────────
        let slots_base = ring_base + ring_header_size;
        for (i, rec_bytes) in records.iter().enumerate() {
            let off = slots_base + i * record_size;
            buf[off..off + record_size].copy_from_slice(rec_bytes);
        }

        buf
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Synthetic roundtrip:
    /// 2 valid records (SpanBegin + SpanEnd) + 1 stomped-magic record.
    /// Expected output: 2 Record events + 1 Repair event.
    #[test]
    fn synthetic_roundtrip() {
        let span_begin = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::SpanBegin as u8,
            TxTraceLevel::Drive as u8,
            0, 1, 1000, 0xaabb, 0, 0x1, TxPayloadTag::None as u16, 0, [0u8; 16],
        );
        let span_end = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::SpanEnd as u8,
            TxTraceLevel::Drive as u8,
            0, 2, 2000, 0xaabb, 0, 0x1, TxPayloadTag::None as u16, 0, [0u8; 16],
        );
        let stomped = make_record_bytes(
            0xDEAD, // bad magic
            0,
            TxTraceKind::SpanBegin as u8,
            TxTraceLevel::Drive as u8,
            0, 3, 3000, 0xccdd, 0, 0x2, TxPayloadTag::None as u16, 0, [0u8; 16],
        );

        let file_bytes = make_trace_file(&[span_begin, span_end, stomped]);
        let events = decode_file_bytes(&file_bytes).expect("decode_file_bytes failed");

        assert_eq!(events.len(), 3, "expected 3 events, got {}", events.len());

        // First two should be Records.
        for i in 0..2 {
            match &events[i] {
                DecodedEvent::Record(r) => {
                    let expected_kind = if i == 0 { "SpanBegin" } else { "SpanEnd" };
                    assert_eq!(r.kind, expected_kind, "event[{i}] kind mismatch");
                }
                DecodedEvent::Repair(r) => panic!("event[{i}] should be Record, got Repair: {r:?}"),
            }
        }

        // Third should be a Repair(bad_magic).
        match &events[2] {
            DecodedEvent::Repair(r) => {
                assert_eq!(
                    r.category, "txtrace.repair.bad_magic",
                    "wrong repair category: {}",
                    r.category
                );
            }
            DecodedEvent::Record(r) => panic!("event[2] should be Repair, got Record: {r:?}"),
        }
    }

    #[test]
    fn bad_header_magic_returns_error() {
        let mut bytes = make_trace_file(&[]);
        // Stomp the header magic.
        write_u32_le(&mut bytes, 0, 0xDEADBEEF);
        let result = decode_file_bytes(&bytes);
        assert!(result.is_err(), "expected error for bad header magic");
        let msg = result.unwrap_err();
        assert!(msg.contains("bad header magic"), "unexpected error: {msg}");
    }

    #[test]
    fn unsupported_header_version_returns_error() {
        let mut bytes = make_trace_file(&[]);
        write_u16_le(&mut bytes, 4, 99); // version = 99
        let result = decode_file_bytes(&bytes);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("unsupported header version"), "unexpected error: {msg}");
    }
}
