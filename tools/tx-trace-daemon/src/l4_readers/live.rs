//! Live guest-memory reader for L4 raw-record capture.
//!
//! This module owns QEMU guest RAM symbol resolution, ring draining, and
//! rawrecords finalization. File replay remains in `replay.rs`; both paths
//! produce the same L4 integrity metadata and feed L5 through `RawRecordFrame`.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::Path;
use std::time::{Duration, Instant};

use flate2::write::GzEncoder;
use flate2::Compression;
use memmap2::MmapOptions;
use object::{Object, ObjectSymbol};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tx_observe_types::{TxTraceHartRing, TxTraceRecord};

use crate::decode::DecodedEvent;
use crate::l4_readers::{RawRecordFrame, TraceInputKind, TraceIntegrity};
use crate::l5_canonical::decode::decode_frame;
use crate::l5_canonical::TraceEventStream;
use crate::perfetto::writer::PftraceWriter;

use super::replay::{
    read_u64_le, RingStats, TraceStats, MAX_HARTS_DAEMON, RING_CONSUMER_OFF, RING_LOST_OFF,
    RING_PRODUCER_OFF, SUPPORTED_HEADER_VERSION,
};

const RV64_QEMU_RAM_BASE: u64 = 0x8000_0000;
const RV64_QEMU_KERNEL_PHYS_BASE: u64 = 0x8020_0000;
const RV64_QEMU_KERNEL_VIRT_BASE: u64 = 0xffff_ffff_8020_0000;
const DEFAULT_LIVE_POLL_MS: u64 = 2;
pub(super) const RING_SEQ_OFF: usize = 200;
pub(super) const LIVE_RAW_RECORD_HEADER_BYTES: usize = 8;

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

    let raw_staging_path = config.out_dir.join(".trace.rawrecords.tmp");
    let raw_path = config.out_dir.join("trace.rawrecords.gz");
    let mut raw = BufWriter::new(File::create(&raw_staging_path)?);

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
    drop(raw);

    if config.finalize {
        let ndjson_path = config.out_dir.join("replay.ndjson");
        let pftrace_path = config.out_dir.join("trace.pftrace");
        finalize_live_raw_records(
            &raw_staging_path,
            &ndjson_path,
            &pftrace_path,
            config.names_map,
            &mut totals,
        )?;
    }
    let raw_capture = compress_raw_records(&raw_staging_path, &raw_path)?;

    let stats = ring_layout.stats_from_mmap(&mmap)?;
    let stream_metadata = empty_stream_from_stats(TraceInputKind::LiveGuestMem, &stats);
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
            "raw_records": "trace.rawrecords.gz",
            "replay_ndjson": if config.finalize { Some("replay.ndjson") } else { None },
            "pftrace": if config.finalize { Some("trace.pftrace") } else { None },
        },
        "raw_capture": raw_capture,
        "drained": totals,
        "stats": stats,
        "integrity": stream_metadata.integrity(),
        "markers": stream_metadata.markers(),
    });
    fs::write(
        runtime_path,
        serde_json::to_vec_pretty(&runtime)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
    )?;

    Ok(stats)
}

#[derive(Serialize)]
pub(super) struct RawCaptureMetadata {
    pub(super) encoding: &'static str,
    pub(super) uncompressed_bytes: u64,
    pub(super) compressed_bytes: u64,
    pub(super) sha256: String,
}

pub(super) fn compress_raw_records(
    raw_path: &Path,
    gzip_path: &Path,
) -> std::io::Result<RawCaptureMetadata> {
    let tmp_path = gzip_path.with_extension("gz.tmp");
    let mut source = BufReader::new(File::open(raw_path)?);
    let target = BufWriter::new(File::create(&tmp_path)?);
    let mut encoder = GzEncoder::new(target, Compression::fast());
    let mut hasher = Sha256::new();
    let mut uncompressed_bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        encoder.write_all(&buffer[..read])?;
        uncompressed_bytes += read as u64;
    }
    encoder.finish()?.flush()?;
    fs::rename(&tmp_path, gzip_path)?;
    fs::remove_file(raw_path)?;

    Ok(RawCaptureMetadata {
        encoding: "gzip",
        uncompressed_bytes,
        compressed_bytes: fs::metadata(gzip_path)?.len(),
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn empty_stream_from_stats(input_kind: TraceInputKind, stats: &TraceStats) -> TraceEventStream {
    TraceEventStream::new(
        Vec::new(),
        TraceIntegrity::from_trace_stats(input_kind, stats),
    )
}

#[derive(Default, Serialize)]
pub(super) struct LiveDrainTotals {
    pub(super) records: u64,
    pub(super) repairs: u64,
    pub(super) overwritten_records: u64,
    pub(super) lost_records: u64,
    pub(super) raw_records: u64,
    #[serde(skip)]
    pub(super) last_lost_by_hart: Vec<u64>,
    #[serde(skip)]
    pub(super) active_harts_mask: u64,
}

pub(super) struct LiveRingLayout {
    ring_offset: usize,
    hart_count: usize,
    pub(super) ring_bytes: usize,
    pub(super) slot_count: usize,
}

impl LiveRingLayout {
    pub(super) fn new(
        ring_offset: usize,
        hart_count: usize,
        ring_bytes: usize,
    ) -> std::io::Result<Self> {
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

pub(super) fn drain_live_once(
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

pub(super) fn finalize_live_raw_records(
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
        let frame = RawRecordFrame::from_slot(ring_hart, None, &slot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let event = decode_frame(&frame);
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
        let frame = RawRecordFrame::from_slot(ring_hart, None, &slot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        events.push(decode_frame(&frame));
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

fn event_order_key(event: &DecodedEvent) -> (u64, u16, u64, u8) {
    match event {
        DecodedEvent::Record(record) => (record.ts, record.hart, record.seq, 0),
        DecodedEvent::Repair(repair) => (u64::MAX, repair.hart, repair.seq_around, 1),
    }
}
