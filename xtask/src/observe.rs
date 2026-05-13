//! `cargo xtask observe` — observation pipeline wrappers.
//!
//! Subcommands:
//! - `replay` — decode a `.txtrace` file and emit NDJSON (default) or Perfetto.
//! - `pftrace` — convenience alias: replay with `--out pftrace --output <path>`.
//! - `validate` — parse header + walk slots, print a one-line summary.
//! - `demo` — generate a small synthetic `.txtrace` file for testing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::optional_option_value;
use crate::Result;

/// Packed demo-record descriptor: (kind, level, name, span, parent, payload_tag, payload_len, payload).
type DemoRecord = (u8, u8, u32, u64, u64, u16, u16, [u8; 16]);

pub(crate) fn observe(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(subcmd) = args.first() else {
        return Err(
            "observe command needs replay, pftrace, validate, or demo\n\
             usage:\n\
             \tcargo xtask observe replay --file <path> [--out json|pftrace] [--output <out>] [--filter level=N]\n\
             \tcargo xtask observe pftrace --file <path> --output <pftrace>\n\
             \tcargo xtask observe validate --file <path>\n\
             \tcargo xtask observe demo --output <path> [--records N] [--with-yields]"
                .into(),
        );
    };
    match subcmd.as_str() {
        "replay" => observe_replay(root, &args[1..]),
        "pftrace" => observe_pftrace(root, &args[1..]),
        "validate" => observe_validate(&args[1..]),
        "demo" => observe_demo(&args[1..]),
        other => Err(format!(
            "unknown observe subcommand '{other}'; expected replay, pftrace, validate, or demo"
        )),
    }
}

// ── Daemon binary helpers ─────────────────────────────────────────────────────

fn daemon_dir(root: &Path) -> PathBuf {
    root.join("tools/tx-trace-daemon")
}

/// Path to the compiled `tx-trace-daemon` binary.
///
/// The daemon lives in its own workspace under `tools/tx-trace-daemon/`, but
/// the root `.cargo/config.toml` sets `target-dir = "target"` (an absolute-or-
/// relative path resolved from each workspace's root).  When cargo processes the
/// daemon workspace it inherits the parent `.cargo/config.toml` and therefore
/// places outputs in the *root* workspace `target/` directory, not in a
/// `tools/tx-trace-daemon/target/` subdirectory.
fn daemon_bin(root: &Path) -> PathBuf {
    root.join("target/debug/tx-trace-daemon")
}

/// Ensure `tx-trace-daemon` binary is built.
///
/// Drives `cargo build` from the daemon's own workspace directory so that its
/// independent `Cargo.toml` / `[workspace]` is respected, while the inherited
/// `.cargo/config.toml` `target-dir = "target"` still routes the output to the
/// root `target/` directory.
fn ensure_daemon_built(root: &Path) -> Result<()> {
    if daemon_bin(root).exists() {
        // Already built; skip rebuild for speed in repeated invocations.
        return Ok(());
    }
    eprintln!("observe: building tx-trace-daemon ...");
    let status = Command::new("cargo")
        .args(["build"])
        .current_dir(daemon_dir(root))
        .status()
        .map_err(|err| format!("failed to run cargo build for tx-trace-daemon: {err}"))?;
    if !status.success() {
        return Err("tx-trace-daemon build failed".into());
    }
    let bin = daemon_bin(root);
    if !bin.exists() {
        return Err(format!(
            "tx-trace-daemon binary not found at {} after build; \
             check that the daemon workspace's Cargo.toml is correct",
            bin.display()
        ));
    }
    Ok(())
}

// ── replay ────────────────────────────────────────────────────────────────────

fn observe_replay(root: &Path, args: &[String]) -> Result<()> {
    let file = require_file_arg(args)?;
    let out_format = optional_option_value(args, "--out").unwrap_or_else(|| "json".to_string());
    let output = optional_option_value(args, "--output");
    let filter = optional_option_value(args, "--filter");

    ensure_daemon_built(root)?;

    let mut cmd_args = vec![
        "replay".to_string(),
        "--file".to_string(),
        file.to_string_lossy().into_owned(),
        "--out".to_string(),
        out_format.clone(),
    ];

    if let Some(out) = &output {
        cmd_args.push("--output".to_string());
        cmd_args.push(out.clone());
    }
    if let Some(f) = &filter {
        cmd_args.push("--filter".to_string());
        cmd_args.push(f.clone());
    }

    if out_format == "pftrace" && output.is_none() {
        return Err("--output is required when --out pftrace".into());
    }

    eprintln!(
        "observe: tx-trace-daemon replay --file {} --out {} ...",
        file.display(),
        out_format,
    );

    let status = Command::new(daemon_bin(root))
        .args(&cmd_args)
        .status()
        .map_err(|err| format!("failed to run tx-trace-daemon: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("tx-trace-daemon exited with {status}"))
    }
}

// ── pftrace ───────────────────────────────────────────────────────────────────

fn observe_pftrace(root: &Path, args: &[String]) -> Result<()> {
    let file = require_file_arg(args)?;
    let output = optional_option_value(args, "--output")
        .ok_or("--output is required for pftrace subcommand")?;

    ensure_daemon_built(root)?;

    eprintln!(
        "observe: tx-trace-daemon replay --file {} --out pftrace --output {} ...",
        file.display(),
        output,
    );

    let status = Command::new(daemon_bin(root))
        .args([
            "replay",
            "--file",
            &file.to_string_lossy(),
            "--out",
            "pftrace",
            "--output",
            &output,
        ])
        .status()
        .map_err(|err| format!("failed to run tx-trace-daemon: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("tx-trace-daemon exited with {status}"))
    }
}

// ── validate ──────────────────────────────────────────────────────────────────

/// Parse the file header and walk slots without emitting anything.
///
/// Prints a summary line:
///   `txtrace v0: 1 harts, 16 slots/hart, 4 records, 0 framing errors`
fn observe_validate(args: &[String]) -> Result<()> {
    let file = require_file_arg(args)?;
    let data = fs::read(&file).map_err(|err| format!("cannot read {}: {err}", file.display()))?;

    // ── Header constants (mirrors replay.rs constants) ────────────────────────
    const HEADER_SIZE: usize = 72; // size_of::<TxTraceHeader>()
    const RING_HDR_SIZE: usize = 208; // size_of::<TxTraceHartRing>()
    const RECORD_SIZE: usize = 80; // size_of::<TxTraceRecord>()
    const TX_TRACE_MAGIC: u32 = 0x5254_5854;
    const RECORD_MAGIC: u16 = 0x5254;

    if data.len() < HEADER_SIZE {
        return Err(format!(
            "file too small ({} bytes) for TxTraceHeader",
            data.len()
        ));
    }

    // Read header fields with little-endian byte parsing.
    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if magic != TX_TRACE_MAGIC {
        return Err(format!(
            "bad header magic: expected 0x{TX_TRACE_MAGIC:08x}, got 0x{magic:08x}"
        ));
    }

    let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
    let rings_off = u64::from_le_bytes(data[64..72].try_into().unwrap()) as usize;
    let hart_count = u16::from_le_bytes(data[12..14].try_into().unwrap()) as usize;
    let ring_order = data[14] as u32;
    let record_size_hdr = u16::from_le_bytes(data[10..12].try_into().unwrap()) as usize;

    if record_size_hdr != RECORD_SIZE {
        return Err(format!(
            "unsupported record_size {record_size_hdr}: expected {RECORD_SIZE}"
        ));
    }
    if !(2..=24).contains(&ring_order) {
        return Err(format!(
            "ring_order {ring_order} is outside the valid range [2, 24]"
        ));
    }

    let slot_count = 1usize << ring_order;
    let ring_data_size = RING_HDR_SIZE + slot_count * RECORD_SIZE;
    let required = rings_off + hart_count * ring_data_size;
    if data.len() < required {
        return Err(format!(
            "file too small: need {required} bytes, got {}",
            data.len()
        ));
    }

    // Walk all slots and count records + framing errors.
    let mut total_records = 0usize;
    let mut framing_errors = 0usize;

    // Ring producer/consumer offsets (matching replay.rs RING_PRODUCER_OFF/RING_CONSUMER_OFF)
    const RING_PRODUCER_OFF: usize = 64;
    const RING_CONSUMER_OFF: usize = 128;

    for h in 0..hart_count {
        let ring_base = rings_off + h * ring_data_size;
        let ring_bytes = &data[ring_base..ring_base + RING_HDR_SIZE];

        let producer = u64::from_le_bytes(
            ring_bytes[RING_PRODUCER_OFF..RING_PRODUCER_OFF + 8]
                .try_into()
                .unwrap(),
        );
        let consumer = u64::from_le_bytes(
            ring_bytes[RING_CONSUMER_OFF..RING_CONSUMER_OFF + 8]
                .try_into()
                .unwrap(),
        );
        let effective_consumer = if producer.wrapping_sub(consumer) > slot_count as u64 {
            producer.wrapping_sub(slot_count as u64)
        } else {
            consumer
        };

        let slots_base = ring_base + RING_HDR_SIZE;
        let mut c = effective_consumer;
        while c != producer {
            let slot_idx = (c & (slot_count as u64 - 1)) as usize;
            let slot_off = slots_base + slot_idx * RECORD_SIZE;
            let slot_bytes = &data[slot_off..slot_off + RECORD_SIZE];
            let rec_magic = u16::from_le_bytes(slot_bytes[0..2].try_into().unwrap());
            if rec_magic == RECORD_MAGIC {
                total_records += 1;
            } else {
                framing_errors += 1;
            }
            c = c.wrapping_add(1);
        }
    }

    println!(
        "txtrace v{version}: {hart_count} hart{}, {slot_count} slots/hart, {total_records} record{}, {framing_errors} framing error{}",
        if hart_count == 1 { "" } else { "s" },
        if total_records == 1 { "" } else { "s" },
        if framing_errors == 1 { "" } else { "s" },
    );
    Ok(())
}

// ── demo ──────────────────────────────────────────────────────────────────────

/// Generate a synthetic `.txtrace` file containing a known mix of records.
///
/// Records emitted (in order):
///   1. SpanBegin  (level=Drive,    name=0x0000_0001, span=0x0000_0001)
///   2. Instant    (level=Boundary, name=0x0000_0002, payload_tag=WaitSourceNotify)
///   3. Counter    (level=Boundary, name=0x0000_0003, payload_tag=CounterValue, value=42)
///   4. SpanEnd    (level=Boundary, name=0x0000_0000, span=0x0000_0001)
///
/// With `--with-yields`, additionally appends:
///   5. Instant    (level=Yield,    name=0x0000_0004, payload_tag=YieldBegin)
///   6. Instant    (level=Yield,    name=0x0000_0005, payload_tag=Resume)
///
/// The file is valid `txtrace v0` and round-trips through `observe validate`
/// and `observe replay --out json`.
///
/// Ring layout chosen so slot_count=16 (ring_order=4), 1 hart.
fn observe_demo(args: &[String]) -> Result<()> {
    let output = optional_option_value(args, "--output")
        .ok_or("--output is required for demo subcommand")?;
    let output_path = PathBuf::from(&output);

    let records_arg: Option<usize> =
        optional_option_value(args, "--records").and_then(|v| v.parse().ok());
    let with_yields = args.iter().any(|a| a == "--with-yields");

    // ── Layout constants ──────────────────────────────────────────────────────
    const TX_TRACE_MAGIC: u32 = 0x5254_5854;
    const RECORD_MAGIC: u16 = 0x5254;
    const RECORD_VERSION: u8 = 0;
    const HEADER_SIZE: usize = 72;
    const RING_HDR_SIZE: usize = 208;
    const RECORD_SIZE: usize = 80;
    const RING_ORDER: u8 = 4; // 16 slots
    const SLOT_COUNT: usize = 1 << RING_ORDER;
    const RINGS_OFF: usize = HEADER_SIZE;
    const RING_PRODUCER_OFF: usize = 64;

    // ── Build record list ─────────────────────────────────────────────────────
    let span_id: u64 = 0x0000_0000_0000_0001;

    // Base records
    let mut record_payloads: Vec<DemoRecord> = vec![
        // (kind, level, name, span, parent, payload_tag, payload_len, payload)
        // 1. SpanBegin
        (
            10u8, // SpanBegin
            2u8,  // Drive
            0x0000_0001u32,
            span_id,
            0u64,
            0u16, // TxPayloadTag::None
            0u16,
            [0u8; 16],
        ),
        // 2. Instant — WaitSourceNotify
        {
            // PayloadWaitSourceNotify: source_id_low=0x1234, mask_bits=0b11, task_id_low=0, wait_gen_low=7
            let mut payload = [0u8; 16];
            payload[0..4].copy_from_slice(&0x1234u32.to_le_bytes());
            payload[4..8].copy_from_slice(&0b11u32.to_le_bytes());
            payload[8..12].copy_from_slice(&0u32.to_le_bytes());
            payload[12..16].copy_from_slice(&7u32.to_le_bytes());
            (
                12u8, // Instant
                0u8,  // Boundary
                0x0000_0002u32,
                0u64,
                span_id,
                8u16, // TxPayloadTag::WaitSourceNotify
                16u16,
                payload,
            )
        },
        // 3. Counter — CounterValue
        {
            // PayloadCounterValue: counter_id=3, _pad=0, value=42
            let mut payload = [0u8; 16];
            payload[0..4].copy_from_slice(&3u32.to_le_bytes()); // counter_id
                                                                // _pad = 0 (bytes 4..8)
            payload[8..16].copy_from_slice(&42u64.to_le_bytes()); // value
            (
                13u8, // Counter
                0u8,  // Boundary
                0x0000_0003u32,
                0u64,
                0u64,
                11u16, // TxPayloadTag::CounterValue
                16u16,
                payload,
            )
        },
        // 4. SpanEnd
        (
            11u8, // SpanEnd
            0u8,  // Boundary
            0x0000_0000u32,
            span_id,
            0u64,
            0u16, // TxPayloadTag::None
            0u16,
            [0u8; 16],
        ),
    ];

    // Optional yield/resume pair
    if with_yields {
        // 5. Instant — YieldBegin
        let mut yield_payload = [0u8; 16];
        yield_payload[0] = 0u8; // shape_kind
        yield_payload[4..8].copy_from_slice(&0x99u32.to_le_bytes()); // task_id_low
        yield_payload[8..16].copy_from_slice(&1u64.to_le_bytes()); // wait_generation
        record_payloads.push((
            12u8, // Instant
            3u8,  // Yield
            0x0000_0004u32,
            0u64,
            span_id,
            6u16, // TxPayloadTag::YieldBegin
            16u16,
            yield_payload,
        ));

        // 6. Instant — Resume
        let mut resume_payload = [0u8; 16];
        resume_payload[0] = 0u8; // resume_kind
        resume_payload[1] = 0u8; // abort_reason
        resume_payload[4..8].copy_from_slice(&0x99u32.to_le_bytes()); // object_id_low
        resume_payload[8..16].copy_from_slice(&1u64.to_le_bytes()); // wait_generation
        record_payloads.push((
            12u8, // Instant
            3u8,  // Yield
            0x0000_0005u32,
            0u64,
            span_id,
            7u16, // TxPayloadTag::Resume
            16u16,
            resume_payload,
        ));
    }

    // Apply --records override (truncate or pad with Instant records)
    if let Some(n) = records_arg {
        record_payloads.truncate(n);
        while record_payloads.len() < n && record_payloads.len() < SLOT_COUNT {
            record_payloads.push((12u8, 0u8, 0x0000_0099u32, 0u64, 0u64, 0u16, 0u16, [0u8; 16]));
        }
    }

    let num_records = record_payloads.len().min(SLOT_COUNT);
    let total_size = RINGS_OFF + RING_HDR_SIZE + SLOT_COUNT * RECORD_SIZE;
    let mut buf = vec![0u8; total_size];

    // ── TxTraceHeader (72 bytes at offset 0) ──────────────────────────────────
    buf[0..4].copy_from_slice(&TX_TRACE_MAGIC.to_le_bytes());
    buf[4..6].copy_from_slice(&0u16.to_le_bytes()); // version = 0
    buf[6..8].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes()); // header_len
    buf[8] = 1u8; // endian = LE
    buf[9] = 8u8; // ptr_width = 8
    buf[10..12].copy_from_slice(&(RECORD_SIZE as u16).to_le_bytes()); // record_size = 80
    buf[12..14].copy_from_slice(&1u16.to_le_bytes()); // hart_count = 1
    buf[14] = RING_ORDER; // ring_order = 4
    buf[15] = 0u8; // flags
                   // _pad0 (16-19) = 0
                   // boot_id (24-31) = 0
                   // clock_id (32-35) = 0 (Unknown)
                   // _pad1 (36-39) = 0
                   // clock_freq_hz (40-47) = 0
                   // string_table_off (48-55) = 0
                   // string_table_len (56-63) = 0
    buf[64..72].copy_from_slice(&(RINGS_OFF as u64).to_le_bytes()); // rings_off

    // ── TxTraceHartRing (208 bytes at offset 72) ──────────────────────────────
    // hart_id = 0, flags = 0 (bytes 0-3 of ring)
    let ring_base = RINGS_OFF;
    // producer at ring_base + RING_PRODUCER_OFF:
    buf[ring_base + RING_PRODUCER_OFF..ring_base + RING_PRODUCER_OFF + 8]
        .copy_from_slice(&(num_records as u64).to_le_bytes());
    // consumer at ring_base + 128 = 0 (already zeroed)

    // ── Record slots (starting at ring_base + RING_HDR_SIZE) ─────────────────
    let slots_base = ring_base + RING_HDR_SIZE;
    for (i, &(kind, level, name, span, parent, payload_tag, payload_len, payload)) in
        record_payloads[..num_records].iter().enumerate()
    {
        let off = slots_base + i * RECORD_SIZE;
        let slot = &mut buf[off..off + RECORD_SIZE];
        // Write all fields at their documented offsets (see record.rs layout).
        slot[0..2].copy_from_slice(&RECORD_MAGIC.to_le_bytes()); // magic
        slot[2] = RECORD_VERSION; // version
        slot[3] = kind; // kind
        slot[4] = level; // level
        slot[5] = 0u8; // flags
        slot[6] = 0u8; // arg_count
        slot[7] = 0u8; // _pad0
        slot[8..10].copy_from_slice(&0u16.to_le_bytes()); // hart = 0
        slot[10..12].copy_from_slice(&0u16.to_le_bytes()); // _pad1
        slot[12..16].copy_from_slice(&0u32.to_le_bytes()); // _pad2
        slot[16..24].copy_from_slice(&(i as u64).to_le_bytes()); // seq
        slot[24..32].copy_from_slice(&((i as u64 + 1) * 1000).to_le_bytes()); // ts
        slot[32..40].copy_from_slice(&span.to_le_bytes()); // span
        slot[40..48].copy_from_slice(&parent.to_le_bytes()); // parent
        slot[48..52].copy_from_slice(&name.to_le_bytes()); // name
        slot[52..54].copy_from_slice(&payload_tag.to_le_bytes()); // payload_tag
        slot[54..56].copy_from_slice(&payload_len.to_le_bytes()); // payload_len
        slot[56..72].copy_from_slice(&payload); // payload
                                                // _pad3 (72-79) = 0 (already zeroed)
    }

    fs::write(&output_path, &buf)
        .map_err(|err| format!("failed to write {}: {err}", output_path.display()))?;

    eprintln!(
        "observe: demo file written to {} ({} bytes, {} records{})",
        output_path.display(),
        buf.len(),
        num_records,
        if with_yields { ", with yields" } else { "" }
    );
    Ok(())
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn require_file_arg(args: &[String]) -> Result<PathBuf> {
    let path_str = optional_option_value(args, "--file").ok_or("--file is required")?;
    let path = PathBuf::from(&path_str);
    if !path.exists() {
        return Err(format!(
            "file not found: {}; check the path and try again",
            path.display()
        ));
    }
    Ok(path)
}
