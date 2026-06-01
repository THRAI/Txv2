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
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::Path;
use std::time::{Duration, Instant};

use memmap2::MmapOptions;
use object::{Object, ObjectSymbol};
use serde::Serialize;
use tx_observe_types::header::TX_TRACE_MAGIC;
use tx_observe_types::{TxTraceHartRing, TxTraceHeader, TxTraceRecord};

use crate::decode::{decode_slot, DecodedEvent};
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
pub(crate) const RING_LOST_OFF: usize = 192;
const RING_SEQ_OFF: usize = 200;

const RECORD_MAGIC: u16 = 0x5254;
const RV64_QEMU_RAM_BASE: u64 = 0x8000_0000;
const RV64_QEMU_KERNEL_PHYS_BASE: u64 = 0x8020_0000;
const RV64_QEMU_KERNEL_VIRT_BASE: u64 = 0xffff_ffff_8020_0000;
const DEFAULT_LIVE_POLL_MS: u64 = 2;
const LIVE_RAW_RECORD_HEADER_BYTES: usize = 8;

#[derive(Clone, Debug, Serialize)]
pub struct RingStats {
    pub hart: u16,
    pub producer: u64,
    pub consumer: u64,
    pub effective_consumer: u64,
    pub visible_records: u64,
    pub overwritten_records: u64,
    pub lost: u64,
    pub framing_errors: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TraceStats {
    pub version: u16,
    pub hart_count: u16,
    pub ring_order: u8,
    pub slots_per_hart: u64,
    pub total_records: u64,
    pub total_lost: u64,
    pub overwritten_records: u64,
    pub framing_errors: u64,
    pub complete: bool,
    pub rings: Vec<RingStats>,
}

pub struct LiveDrainConfig<'a> {
    pub guest_mem: &'a Path,
    pub kernel: &'a Path,
    pub out_dir: &'a Path,
    pub symbol: &'a str,
    pub hart_count: usize,
    pub ring_bytes: usize,
    pub stop_file: Option<&'a Path>,
    pub poll_ms: u64,
    pub max_duration_ms: Option<u64>,
    pub finalize: bool,
    pub names_map: Option<HashMap<u32, String>>,
}

/// Run the file-replay transport.
///
/// Opens `path`, validates the header, then iterates over all hart rings and
/// decodes every filled slot.  Each decoded event is emitted as a
/// newline-delimited JSON record.
///
/// If `min_level` is `Some(n)`, records whose `level` byte is < n are dropped.
pub fn run(path: &Path, min_level: Option<u8>) -> std::io::Result<()> {
    let data = std::fs::read(path)?;
    let events = decode_file_bytes(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

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
    let (clock_id, clock_freq_hz, boot_id) = read_header_meta(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let events = decode_file_bytes(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

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

pub fn write_bundle(
    path: &Path,
    out_dir: &Path,
    names_map: Option<HashMap<u32, String>>,
    names_source: Option<&Path>,
) -> std::io::Result<TraceStats> {
    fs::create_dir_all(out_dir)?;

    let trace_path = out_dir.join("trace.txtrace");
    if path != trace_path {
        fs::copy(path, &trace_path)?;
    }

    if let Some(src) = names_source {
        let names_path = out_dir.join("names.json");
        if src != names_path {
            fs::copy(src, &names_path)?;
        }
    }

    let data = fs::read(&trace_path)?;
    let stats = trace_stats_from_bytes(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let events = decode_file_bytes(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let ndjson_path = out_dir.join("replay.ndjson");
    let mut ndjson = BufWriter::new(File::create(&ndjson_path)?);
    for event in &events {
        serde_json::to_writer(&mut ndjson, event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        ndjson.write_all(b"\n")?;
    }
    ndjson.flush()?;

    let pftrace_path = out_dir.join("trace.pftrace");
    run_pftrace(&trace_path, &pftrace_path, names_map)?;

    let runtime_path = out_dir.join("runtime.json");
    let generated_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let runtime = serde_json::json!({
        "schema": "tx-observe-runtime-v0",
        "generated_unix_ms": generated_unix_ms,
        "input": path.display().to_string(),
        "files": {
            "trace": "trace.txtrace",
            "replay_ndjson": "replay.ndjson",
            "pftrace": "trace.pftrace",
            "names": names_source.map(|_| "names.json"),
        },
        "stats": stats,
    });
    fs::write(
        runtime_path,
        serde_json::to_vec_pretty(&runtime)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
    )?;

    Ok(stats)
}

pub fn run_live_guest_mem(config: LiveDrainConfig<'_>) -> std::io::Result<TraceStats> {
    fs::create_dir_all(config.out_dir)?;

    let symbol_addr = resolve_elf_symbol(config.kernel, config.symbol)?;
    let ring_offset = guest_ram_offset_for_symbol(
        symbol_addr,
        RV64_QEMU_KERNEL_VIRT_BASE,
        RV64_QEMU_KERNEL_PHYS_BASE,
    )
    .ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "symbol {} address 0x{symbol_addr:x} is not in the rv64-qemu kernel alias window",
                config.symbol
            ),
        )
    })?;

    let ring_layout =
        LiveRingLayout::new(ring_offset as usize, config.hart_count, config.ring_bytes)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.guest_mem)?;
    let mut mmap = unsafe { MmapOptions::new().map_mut(&file)? };
    ring_layout.validate_len(mmap.len())?;

    let raw_path = config.out_dir.join("trace.rawrecords");
    let mut raw = BufWriter::new(File::create(&raw_path)?);

    let poll = Duration::from_millis(if config.poll_ms == 0 {
        DEFAULT_LIVE_POLL_MS
    } else {
        config.poll_ms
    });
    let deadline = config
        .max_duration_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut totals = LiveDrainTotals::default();

    loop {
        let drained = drain_live_once(&mut mmap, &ring_layout, &mut raw, &mut totals)?;
        let stop_requested = config.stop_file.is_some_and(|path| path.exists());
        let timed_out = deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if (stop_requested && drained == 0) || timed_out {
            break;
        }
        std::thread::sleep(poll);
    }
    raw.flush()?;

    if config.finalize {
        let ndjson_path = config.out_dir.join("replay.ndjson");
        let pftrace_path = config.out_dir.join("trace.pftrace");
        finalize_live_raw_records(
            &raw_path,
            &ndjson_path,
            &pftrace_path,
            config.names_map,
            &mut totals,
        )?;
    }

    let stats = ring_layout.stats_from_mmap(&mmap)?;
    let runtime_path = config.out_dir.join("runtime.json");
    let generated_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let runtime = serde_json::json!({
        "schema": "tx-observe-live-runtime-v0",
        "generated_unix_ms": generated_unix_ms,
        "mode": "guest-mem-live",
        "guest_mem": config.guest_mem.display().to_string(),
        "kernel": config.kernel.display().to_string(),
        "symbol": config.symbol,
        "symbol_addr": format!("0x{symbol_addr:x}"),
        "ring_offset": ring_offset,
        "ring_bytes": config.ring_bytes,
        "hart_count": config.hart_count,
        "finalized": config.finalize,
        "files": {
            "raw_records": "trace.rawrecords",
            "replay_ndjson": if config.finalize { Some("replay.ndjson") } else { None },
            "pftrace": if config.finalize { Some("trace.pftrace") } else { None },
        },
        "drained": totals,
        "stats": stats,
    });
    fs::write(
        runtime_path,
        serde_json::to_vec_pretty(&runtime)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
    )?;

    Ok(stats)
}

#[derive(Default, Serialize)]
struct LiveDrainTotals {
    records: u64,
    repairs: u64,
    overwritten_records: u64,
    lost_records: u64,
    raw_records: u64,
    #[serde(skip)]
    last_lost_by_hart: Vec<u64>,
    #[serde(skip)]
    active_harts_mask: u64,
}

struct LiveRingLayout {
    ring_offset: usize,
    hart_count: usize,
    ring_bytes: usize,
    slot_count: usize,
}

impl LiveRingLayout {
    fn new(ring_offset: usize, hart_count: usize, ring_bytes: usize) -> std::io::Result<Self> {
        if hart_count == 0 || hart_count > MAX_HARTS_DAEMON as usize {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("hart_count {hart_count} is outside [1, {MAX_HARTS_DAEMON}]"),
            ));
        }
        if ring_bytes <= size_of::<TxTraceHartRing>() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("ring_bytes {ring_bytes} is too small"),
            ));
        }
        let slot_bytes = ring_bytes - size_of::<TxTraceHartRing>();
        let raw_slots = slot_bytes / size_of::<TxTraceRecord>();
        let slot_count = prev_power_of_two(raw_slots);
        if slot_count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("ring_bytes {ring_bytes} leaves no usable trace slots"),
            ));
        }
        Ok(Self {
            ring_offset,
            hart_count,
            ring_bytes,
            slot_count,
        })
    }

    fn validate_len(&self, len: usize) -> std::io::Result<()> {
        let required = self.ring_offset + self.hart_count * self.ring_bytes;
        if len < required {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("guest memory file too small: need {required} bytes, got {len}"),
            ));
        }
        Ok(())
    }

    fn ring_base(&self, hart: usize) -> usize {
        self.ring_offset + hart * self.ring_bytes
    }

    fn stats_from_mmap(&self, mmap: &[u8]) -> std::io::Result<TraceStats> {
        self.validate_len(mmap.len())?;
        let mut rings = Vec::with_capacity(self.hart_count);
        let mut total_records = 0u64;
        let mut total_lost = 0u64;
        let mut overwritten_records = 0u64;
        let framing_errors = 0u64;
        for h in 0..self.hart_count {
            let ring_base = self.ring_base(h);
            let ring = &mmap[ring_base..ring_base + size_of::<TxTraceHartRing>()];
            let hart = u16::from_le_bytes(ring[0..2].try_into().unwrap());
            let producer = read_u64_le(ring, RING_PRODUCER_OFF);
            let consumer = read_u64_le(ring, RING_CONSUMER_OFF);
            let lost = read_u64_le(ring, RING_LOST_OFF);
            if !self.ring_header_ready(h, ring) {
                rings.push(RingStats {
                    hart: h as u16,
                    producer,
                    consumer,
                    effective_consumer: consumer,
                    visible_records: 0,
                    overwritten_records: 0,
                    lost: 0,
                    framing_errors: 0,
                });
                continue;
            }
            let produced_window = producer.wrapping_sub(consumer);
            let overwritten = produced_window.saturating_sub(self.slot_count as u64);
            let effective_consumer = if overwritten > 0 {
                producer.wrapping_sub(self.slot_count as u64)
            } else {
                consumer
            };
            let visible_records = producer.wrapping_sub(effective_consumer);
            total_records += visible_records;
            total_lost += lost;
            overwritten_records += overwritten;
            rings.push(RingStats {
                hart,
                producer,
                consumer,
                effective_consumer,
                visible_records,
                overwritten_records: overwritten,
                lost,
                framing_errors: 0,
            });
        }
        Ok(TraceStats {
            version: SUPPORTED_HEADER_VERSION,
            hart_count: self.hart_count as u16,
            ring_order: self.slot_count.trailing_zeros() as u8,
            slots_per_hart: self.slot_count as u64,
            total_records,
            total_lost,
            overwritten_records,
            framing_errors,
            complete: total_lost == 0 && overwritten_records == 0 && framing_errors == 0,
            rings,
        })
    }

    fn ring_header_ready(&self, hart_index: usize, ring: &[u8]) -> bool {
        if ring.len() < size_of::<TxTraceHartRing>() {
            return false;
        }
        let ring_hart = u16::from_le_bytes(ring[0..2].try_into().unwrap());
        let flags = u16::from_le_bytes(ring[2..4].try_into().unwrap());
        if usize::from(ring_hart) != hart_index || flags != 0 {
            return false;
        }

        let producer = read_u64_le(ring, RING_PRODUCER_OFF);
        let consumer = read_u64_le(ring, RING_CONSUMER_OFF);
        let seq = read_u64_le(ring, RING_SEQ_OFF);
        if consumer > producer || producer - consumer > self.slot_count as u64 {
            return false;
        }

        // In a valid initialized ring, seq tracks successful record
        // publication. A concurrent producer may have reserved the next seq
        // before publishing producer+1, so allow that one-record transient.
        seq == producer || seq == producer.saturating_add(1)
    }
}

fn drain_live_once(
    mmap: &mut [u8],
    layout: &LiveRingLayout,
    raw: &mut dyn Write,
    totals: &mut LiveDrainTotals,
) -> std::io::Result<u64> {
    let mut drained = 0u64;
    for h in 0..layout.hart_count {
        let ring_base = layout.ring_base(h);
        let ring_end = ring_base + size_of::<TxTraceHartRing>();
        let ring = &mmap[ring_base..ring_end];
        if !layout.ring_header_ready(h, ring) {
            continue;
        }
        let ring_hart = u16::from_le_bytes(ring[0..2].try_into().unwrap());
        let producer = read_u64_le(ring, RING_PRODUCER_OFF);
        let consumer = read_u64_le(ring, RING_CONSUMER_OFF);
        let lost = read_u64_le(ring, RING_LOST_OFF);
        let produced_window = producer.wrapping_sub(consumer);
        let overwritten = produced_window.saturating_sub(layout.slot_count as u64);
        let effective_consumer = if overwritten > 0 {
            totals.overwritten_records += overwritten;
            producer.wrapping_sub(layout.slot_count as u64)
        } else {
            consumer
        };

        let slots_base = ring_end;
        let mut c = effective_consumer;
        let ring_drained_before = drained;
        while c != producer {
            let slot_idx = (c & (layout.slot_count as u64 - 1)) as usize;
            let slot_off = slots_base + slot_idx * size_of::<TxTraceRecord>();
            let slot = &mmap[slot_off..slot_off + size_of::<TxTraceRecord>()];
            raw.write_all(&ring_hart.to_le_bytes())?;
            raw.write_all(&[0u8; LIVE_RAW_RECORD_HEADER_BYTES - 2])?;
            raw.write_all(slot)?;
            totals.raw_records += 1;
            drained += 1;
            c = c.wrapping_add(1);
        }
        if drained != ring_drained_before && ring_hart < 64 {
            totals.active_harts_mask |= 1u64 << ring_hart;
        }
        mmap[ring_base + RING_CONSUMER_OFF..ring_base + RING_CONSUMER_OFF + 8]
            .copy_from_slice(&producer.to_le_bytes());
        if totals.last_lost_by_hart.len() <= h {
            totals.last_lost_by_hart.resize(h + 1, 0);
        }
        let previous_lost = totals.last_lost_by_hart[h];
        totals.lost_records += if lost >= previous_lost {
            lost - previous_lost
        } else {
            lost
        };
        totals.last_lost_by_hart[h] = lost;
    }
    Ok(drained)
}

fn finalize_live_raw_records(
    raw_path: &Path,
    ndjson_path: &Path,
    pftrace_path: &Path,
    names_map: Option<HashMap<u32, String>>,
    totals: &mut LiveDrainTotals,
) -> std::io::Result<()> {
    if totals.active_harts_mask.count_ones() > 1 {
        return finalize_live_raw_records_sorted(
            raw_path,
            ndjson_path,
            pftrace_path,
            names_map,
            totals,
        );
    }

    let mut raw = BufReader::new(File::open(raw_path)?);
    let mut ndjson = BufWriter::new(File::create(ndjson_path)?);
    let mut pftrace = PftraceWriter::new(0, 10_000_000, 0);
    if let Some(names_map) = names_map {
        pftrace.load_names(names_map);
    }

    let mut header = [0u8; LIVE_RAW_RECORD_HEADER_BYTES];
    let mut slot = [0u8; size_of::<TxTraceRecord>()];
    loop {
        match raw.read_exact(&mut header) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(err),
        }
        raw.read_exact(&mut slot)?;
        let ring_hart = u16::from_le_bytes(header[0..2].try_into().unwrap());
        let event = decode_slot(ring_hart, &slot);
        match &event {
            DecodedEvent::Record(_) => totals.records += 1,
            DecodedEvent::Repair(_) => totals.repairs += 1,
        }
        serde_json::to_writer(&mut ndjson, &event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        ndjson.write_all(b"\n")?;
        pftrace.push(&event);
    }
    ndjson.flush()?;
    pftrace.finish(pftrace_path)
}

fn finalize_live_raw_records_sorted(
    raw_path: &Path,
    ndjson_path: &Path,
    pftrace_path: &Path,
    names_map: Option<HashMap<u32, String>>,
    totals: &mut LiveDrainTotals,
) -> std::io::Result<()> {
    let mut raw = BufReader::new(File::open(raw_path)?);
    let mut events = Vec::new();
    let mut header = [0u8; LIVE_RAW_RECORD_HEADER_BYTES];
    let mut slot = [0u8; size_of::<TxTraceRecord>()];
    loop {
        match raw.read_exact(&mut header) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(err),
        }
        raw.read_exact(&mut slot)?;
        let ring_hart = u16::from_le_bytes(header[0..2].try_into().unwrap());
        events.push(decode_slot(ring_hart, &slot));
    }
    events.sort_by_key(event_order_key);

    let mut ndjson = BufWriter::new(File::create(ndjson_path)?);
    let mut pftrace = PftraceWriter::new(0, 10_000_000, 0);
    if let Some(names_map) = names_map {
        pftrace.load_names(names_map);
    }
    for event in events {
        match &event {
            DecodedEvent::Record(_) => totals.records += 1,
            DecodedEvent::Repair(_) => totals.repairs += 1,
        }
        serde_json::to_writer(&mut ndjson, &event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        ndjson.write_all(b"\n")?;
        pftrace.push(&event);
    }
    ndjson.flush()?;
    pftrace.finish(pftrace_path)
}

pub fn resolve_elf_symbol(path: &Path, symbol: &str) -> std::io::Result<u64> {
    let bytes = fs::read(path)?;
    let file = object::File::parse(bytes.as_slice()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("ELF parse failed: {e}"),
        )
    })?;
    for sym in file.symbols() {
        let Ok(name) = sym.name() else { continue };
        if name == symbol
            || rustc_demangle::try_demangle(name).is_ok_and(|d| format!("{d:#}") == symbol)
        {
            return Ok(sym.address());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("symbol {symbol} not found in {}", path.display()),
    ))
}

pub fn guest_ram_offset_for_symbol(
    symbol_addr: u64,
    kernel_virt_base: u64,
    kernel_phys_base: u64,
) -> Option<u64> {
    let phys = if symbol_addr >= kernel_virt_base {
        kernel_phys_base.checked_add(symbol_addr.checked_sub(kernel_virt_base)?)?
    } else if symbol_addr >= kernel_phys_base {
        symbol_addr
    } else {
        return None;
    };
    phys.checked_sub(RV64_QEMU_RAM_BASE)
}

const fn prev_power_of_two(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    1usize << ((usize::BITS - 1 - n.leading_zeros()) as usize)
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
        let ring_hart = u16::from_le_bytes(
            ring_bytes[0..2]
                .try_into()
                .expect("ring hart id bytes present"),
        );

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
            let event = decode_slot(ring_hart, slot_bytes);
            events.push(event);
            c = c.wrapping_add(1);
        }
    }

    if hart_count > 1 {
        events.sort_by_key(event_order_key);
    }

    Ok(events)
}

pub fn trace_stats_from_bytes(data: &[u8]) -> Result<TraceStats, String> {
    if data.len() < size_of::<TxTraceHeader>() {
        return Err(format!(
            "file too small ({} bytes) for TxTraceHeader ({}) bytes",
            data.len(),
            size_of::<TxTraceHeader>()
        ));
    }

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
    let slot_count = 1usize << (header.ring_order as u32);
    let record_size = size_of::<TxTraceRecord>();
    let ring_header_size = size_of::<TxTraceHartRing>();
    let ring_data_size = ring_header_size + slot_count * record_size;
    let required_size = rings_off + hart_count * ring_data_size;
    if data.len() < required_size {
        return Err(format!(
            "file too small: need at least {required_size} bytes for {} harts \
             (rings_off={rings_off}, ring_data_size={ring_data_size}), got {}",
            hart_count,
            data.len()
        ));
    }

    let mut rings = Vec::with_capacity(hart_count);
    let mut total_records = 0u64;
    let mut total_lost = 0u64;
    let mut overwritten_records = 0u64;
    let mut framing_errors = 0u64;

    for h in 0..hart_count {
        let ring_base = rings_off + h * ring_data_size;
        let ring_bytes = &data[ring_base..ring_base + ring_header_size];
        let hart = u16::from_le_bytes(
            ring_bytes[0..2]
                .try_into()
                .expect("ring hart id bytes present"),
        );
        let producer = read_u64_le(ring_bytes, RING_PRODUCER_OFF);
        let consumer = read_u64_le(ring_bytes, RING_CONSUMER_OFF);
        let lost = read_u64_le(ring_bytes, RING_LOST_OFF);
        let produced_window = producer.wrapping_sub(consumer);
        let overwritten = produced_window.saturating_sub(slot_count as u64);
        let effective_consumer = if overwritten > 0 {
            producer.wrapping_sub(slot_count as u64)
        } else {
            consumer
        };
        let visible_records = producer.wrapping_sub(effective_consumer);

        let slots_base = ring_base + ring_header_size;
        let mut ring_framing_errors = 0u64;
        let mut c = effective_consumer;
        while c != producer {
            let slot_idx = (c & (slot_count as u64 - 1)) as usize;
            let slot_off = slots_base + slot_idx * record_size;
            let slot_bytes = &data[slot_off..slot_off + record_size];
            let rec_magic = u16::from_le_bytes(slot_bytes[0..2].try_into().unwrap());
            if rec_magic != RECORD_MAGIC {
                ring_framing_errors += 1;
            }
            c = c.wrapping_add(1);
        }

        total_records += visible_records;
        total_lost += lost;
        overwritten_records += overwritten;
        framing_errors += ring_framing_errors;
        rings.push(RingStats {
            hart,
            producer,
            consumer,
            effective_consumer,
            visible_records,
            overwritten_records: overwritten,
            lost,
            framing_errors: ring_framing_errors,
        });
    }

    Ok(TraceStats {
        version: header.version,
        hart_count: header.hart_count,
        ring_order: header.ring_order,
        slots_per_hart: slot_count as u64,
        total_records,
        total_lost,
        overwritten_records,
        framing_errors,
        complete: total_lost == 0 && overwritten_records == 0 && framing_errors == 0,
        rings,
    })
}

fn event_order_key(event: &DecodedEvent) -> (u64, u16, u64, u8) {
    match event {
        DecodedEvent::Record(record) => (record.ts, record.hart, record.seq, 0),
        DecodedEvent::Repair(repair) => (u64::MAX, repair.hart, repair.seq_around, 1),
    }
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
        "Script" => 1,
        "Drive" => 2,
        "Yield" => 3,
        "Step" => 4,
        "Phase" => 5,
        "Mutation" => 6,
        _ => 0,
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
        header::TX_TRACE_MAGIC, payload::TxPayloadTag, TxTraceHeader, TxTraceKind, TxTraceLevel,
        TxTraceRecord,
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
        write_u16_le(&mut buf, 0, magic); // magic
        buf[2] = version; // version
        buf[3] = kind; // kind
        buf[4] = level; // level
                        // flags=0, arg_count=0, _pad0=0 at bytes 5,6,7
        write_u16_le(&mut buf, 8, hart); // hart
                                         // _pad1, _pad2 are zero
        write_u64_le(&mut buf, 16, seq); // seq
        write_u64_le(&mut buf, 24, ts); // ts
        write_u64_le(&mut buf, 32, span); // span
        write_u64_le(&mut buf, 40, parent); // parent
        write_u32_le(&mut buf, 48, name); // name
        write_u16_le(&mut buf, 52, payload_tag); // payload_tag
        write_u16_le(&mut buf, 54, payload_len); // payload_len
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
        make_trace_file_multi(&[records.to_vec()])
    }

    /// Build a synthetic multi-hart .txtrace file with interleaved ring layout.
    fn make_trace_file_multi(records_by_hart: &[Vec<Vec<u8>>]) -> Vec<u8> {
        let ring_order: u8 = 4; // 16 slots — more than enough
        let slot_count = 1usize << ring_order;
        let record_size = size_of::<TxTraceRecord>(); // 80
        let ring_header_size = size_of::<TxTraceHartRing>(); // 208
        let rings_off = size_of::<TxTraceHeader>(); // 72
        let ring_data_size = ring_header_size + slot_count * record_size;
        let hart_count = records_by_hart.len();

        let total = rings_off + hart_count * ring_data_size;
        let mut buf = vec![0u8; total];

        // ── TxTraceHeader (72 bytes) ──────────────────────────────────────
        write_u32_le(&mut buf, 0, TX_TRACE_MAGIC); // magic
        write_u16_le(&mut buf, 4, 0); // version = 0
        write_u16_le(&mut buf, 6, rings_off as u16); // header_len
        buf[8] = 1; // endian = LE
        buf[9] = 8; // ptr_width
        write_u16_le(&mut buf, 10, 80); // record_size
        write_u16_le(&mut buf, 12, hart_count as u16); // hart_count
        buf[14] = ring_order; // ring_order
                              // flags = 0, _pad0 = 0 (bytes 15, 16-19)
                              // boot_id at 24 — leave 0
                              // clock_id at 32 — leave 0 (Unknown)
                              // clock_freq_hz at 40 — leave 0
                              // string_table_off at 48, string_table_len at 56 — leave 0
        write_u64_le(&mut buf, 64, rings_off as u64); // rings_off

        for (hart, records) in records_by_hart.iter().enumerate() {
            // ── TxTraceHartRing (208 bytes) ───────────────────────────────
            let ring_base = rings_off + hart * ring_data_size;
            write_u16_le(&mut buf, ring_base, hart as u16); // hart_id
            write_u64_le(
                &mut buf,
                ring_base + RING_PRODUCER_OFF,
                records.len() as u64,
            );
            // consumer at ring_base + RING_CONSUMER_OFF (= 128): 0

            // ── Slot data ─────────────────────────────────────────────────
            let slots_base = ring_base + ring_header_size;
            for (i, rec_bytes) in records.iter().enumerate() {
                let off = slots_base + i * record_size;
                buf[off..off + record_size].copy_from_slice(rec_bytes);
            }
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
            0,
            1,
            1000,
            0xaabb,
            0,
            0x1,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let span_end = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::SpanEnd as u8,
            TxTraceLevel::Drive as u8,
            0,
            2,
            2000,
            0xaabb,
            0,
            0x1,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let stomped = make_record_bytes(
            0xDEAD, // bad magic
            0,
            TxTraceKind::SpanBegin as u8,
            TxTraceLevel::Drive as u8,
            0,
            3,
            3000,
            0xccdd,
            0,
            0x2,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
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
    fn multi_hart_replay_orders_records_by_timestamp() {
        let hart0_late = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::Instant as u8,
            TxTraceLevel::Boundary as u8,
            0,
            1,
            3000,
            0,
            0,
            0x10,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let hart1_early = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::Instant as u8,
            TxTraceLevel::Boundary as u8,
            1,
            1,
            1000,
            0,
            0,
            0x20,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );

        let file_bytes = make_trace_file_multi(&[vec![hart0_late], vec![hart1_early]]);
        let events = decode_file_bytes(&file_bytes).expect("decode_file_bytes failed");

        let records: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                DecodedEvent::Record(record) => Some(record),
                DecodedEvent::Repair(_) => None,
            })
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].hart, 1, "earlier hart-1 record must come first");
        assert_eq!(records[0].ts, 1000);
        assert_eq!(records[1].hart, 0);
        assert_eq!(records[1].ts, 3000);
    }

    #[test]
    fn multi_hart_live_raw_finalize_orders_records_by_timestamp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let raw_path = temp.path().join("trace.rawrecords");
        let ndjson_path = temp.path().join("replay.ndjson");
        let pftrace_path = temp.path().join("trace.pftrace");
        let hart0_late = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::Instant as u8,
            TxTraceLevel::Boundary as u8,
            0,
            1,
            3000,
            0,
            0,
            0x10,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let hart1_early = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::Instant as u8,
            TxTraceLevel::Boundary as u8,
            1,
            1,
            1000,
            0,
            0,
            0x20,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let mut raw = Vec::new();
        raw.extend_from_slice(&0u16.to_le_bytes());
        raw.extend_from_slice(&[0u8; LIVE_RAW_RECORD_HEADER_BYTES - 2]);
        raw.extend_from_slice(&hart0_late);
        raw.extend_from_slice(&1u16.to_le_bytes());
        raw.extend_from_slice(&[0u8; LIVE_RAW_RECORD_HEADER_BYTES - 2]);
        raw.extend_from_slice(&hart1_early);
        fs::write(&raw_path, raw).expect("write raw records");

        let mut totals = LiveDrainTotals {
            active_harts_mask: 0b11,
            ..LiveDrainTotals::default()
        };
        finalize_live_raw_records(&raw_path, &ndjson_path, &pftrace_path, None, &mut totals)
            .expect("finalize live raw records");

        let ndjson = fs::read_to_string(&ndjson_path).expect("read ndjson");
        let events: Vec<serde_json::Value> = ndjson
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect();
        assert_eq!(totals.records, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["hart"], 1);
        assert_eq!(events[0]["ts"], 1000);
        assert_eq!(events[1]["hart"], 0);
        assert_eq!(events[1]["ts"], 3000);
        assert!(pftrace_path.exists());
    }

    #[test]
    fn trace_stats_reports_lost_counter_as_incomplete() {
        let record = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::Instant as u8,
            TxTraceLevel::Boundary as u8,
            0,
            1,
            1000,
            0,
            0,
            0x10,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let mut file_bytes = make_trace_file(&[record]);
        let rings_off = size_of::<TxTraceHeader>();
        write_u64_le(&mut file_bytes, rings_off + RING_LOST_OFF, 3);

        let stats = trace_stats_from_bytes(&file_bytes).expect("trace stats");

        assert_eq!(stats.total_records, 1);
        assert_eq!(stats.total_lost, 3);
        assert!(!stats.complete, "lost records make a ring incomplete");
        assert_eq!(stats.rings[0].lost, 3);
    }

    #[test]
    fn guest_ram_offset_uses_low_kernel_physical_alias_for_high_symbol() {
        assert_eq!(
            guest_ram_offset_for_symbol(0xffff_ffff_8220_1000, 0xffff_ffff_8020_0000, 0x8020_0000),
            Some(0x0220_1000)
        );
    }

    #[test]
    fn live_layout_rounds_raw_slots_down_like_kernel_ring_init() {
        let layout = LiveRingLayout::new(0, 4, 2 * 1024 * 1024).expect("layout");

        assert_eq!(layout.slot_count, 16_384);
        assert_eq!(layout.ring_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn live_drain_advances_consumer_and_writes_raw_record() {
        let ring_order: u8 = 4;
        let slot_count = 1usize << ring_order;
        let ring_bytes = size_of::<TxTraceHartRing>() + slot_count * size_of::<TxTraceRecord>();
        let ring_offset = 128usize;
        let mut mem = vec![0u8; ring_offset + ring_bytes];
        let ring_base = ring_offset;
        write_u16_le(&mut mem, ring_base, 0);
        write_u64_le(&mut mem, ring_base + RING_PRODUCER_OFF, 1);
        write_u64_le(&mut mem, ring_base + RING_SEQ_OFF, 1);

        let record = make_record_bytes(
            RECORD_MAGIC,
            0,
            TxTraceKind::Instant as u8,
            TxTraceLevel::Boundary as u8,
            0,
            1,
            1000,
            0,
            0,
            0x10,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let slot_off = ring_base + size_of::<TxTraceHartRing>();
        mem[slot_off..slot_off + size_of::<TxTraceRecord>()].copy_from_slice(&record);

        let layout = LiveRingLayout::new(ring_offset, 1, ring_bytes).expect("layout");
        let mut raw = Vec::new();
        let mut totals = LiveDrainTotals::default();

        let drained = drain_live_once(&mut mem, &layout, &mut raw, &mut totals).expect("live drain");

        assert_eq!(drained, 1);
        assert_eq!(totals.raw_records, 1);
        assert_eq!(
            read_u64_le(
                &mem[ring_base..ring_base + size_of::<TxTraceHartRing>()],
                RING_CONSUMER_OFF
            ),
            1
        );
        assert_eq!(
            raw.len(),
            LIVE_RAW_RECORD_HEADER_BYTES + size_of::<TxTraceRecord>()
        );
        assert_eq!(&raw[0..2], &0u16.to_le_bytes());
        let decoded = decode_slot(0, &raw[LIVE_RAW_RECORD_HEADER_BYTES..]);
        assert!(matches!(decoded, DecodedEvent::Record(_)));
    }

    #[test]
    fn live_drain_ignores_uninitialized_ring_header() {
        let ring_order: u8 = 4;
        let slot_count = 1usize << ring_order;
        let ring_bytes = size_of::<TxTraceHartRing>() + slot_count * size_of::<TxTraceRecord>();
        let ring_offset = 128usize;
        let mut mem = vec![0u8; ring_offset + ring_bytes];
        let ring_base = ring_offset;

        write_u16_le(&mut mem, ring_base, 0);
        write_u64_le(&mut mem, ring_base + RING_PRODUCER_OFF, 8);
        write_u64_le(&mut mem, ring_base + RING_CONSUMER_OFF, 3);
        write_u64_le(&mut mem, ring_base + RING_SEQ_OFF, 0xfeed_beef);

        let layout = LiveRingLayout::new(ring_offset, 1, ring_bytes).expect("layout");
        let mut raw = Vec::new();
        let mut totals = LiveDrainTotals::default();

        let drained = drain_live_once(&mut mem, &layout, &mut raw, &mut totals).expect("live drain");

        assert_eq!(drained, 0);
        assert_eq!(totals.raw_records, 0);
        assert!(raw.is_empty());
        assert_eq!(
            read_u64_le(
                &mem[ring_base..ring_base + size_of::<TxTraceHartRing>()],
                RING_CONSUMER_OFF
            ),
            3
        );
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
        assert!(
            msg.contains("unsupported header version"),
            "unexpected error: {msg}"
        );
    }
}
