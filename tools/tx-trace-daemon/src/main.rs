//! `tx-trace-daemon` — OBS-5 + OBS-6 decode and Perfetto emission.
//!
//! OBS-5: `replay` subcommand with `--out json` (newline-delimited JSON).
//! OBS-6: `replay` subcommand with `--out pftrace --output <path>` (binary Perfetto trace).

mod decode;
mod emit_json;
mod perfetto;
mod replay;

use clap::{Parser, Subcommand};

/// tx-trace-daemon CLI.
#[derive(Parser)]
#[command(
    name = "tx-trace-daemon",
    about = "Decode and inspect txtrace-v0 kernel trace regions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Replay a captured trace region file.
    ///
    /// With `--out json` (default), emits newline-delimited JSON records to stdout.
    /// With `--out pftrace`, writes a binary Perfetto `.pftrace` file to `--output`.
    Replay {
        /// Path to a captured `.txtrace` region file.
        #[arg(long, short)]
        file: std::path::PathBuf,

        /// Output format: `json` (default) or `pftrace`.
        #[arg(long, default_value = "json")]
        out: String,

        /// Output path for `--out pftrace`.  Required when `--out pftrace`.
        #[arg(long)]
        output: Option<std::path::PathBuf>,

        /// Optional filter clause: `level=<N>` drops records below level N.
        /// Format: `level=<0..6>`.
        #[arg(long)]
        filter: Option<String>,

        /// Optional path to a `names.json` file for EventNameId resolution.
        #[arg(long)]
        names: Option<std::path::PathBuf>,
    },
    /// Build a self-contained runtime directory for one trace.
    ///
    /// Writes trace.txtrace, replay.ndjson, trace.pftrace, optional names.json,
    /// and runtime.json. runtime.json carries ring completeness stats.
    Bundle {
        /// Path to a captured `.txtrace` region file.
        #[arg(long, short)]
        file: std::path::PathBuf,

        /// Directory to create/update with the trace runtime artifacts.
        #[arg(long)]
        out_dir: std::path::PathBuf,

        /// Optional path to a `names.json` file for EventNameId resolution.
        #[arg(long)]
        names: Option<std::path::PathBuf>,
    },
    /// Drain tx-observe rings from a QEMU guest-RAM memory-backend file.
    LiveGuestMem {
        /// Host-visible QEMU guest RAM file.
        #[arg(long)]
        guest_mem: std::path::PathBuf,

        /// Kernel ELF used to locate the exported observation-ring symbol.
        #[arg(long)]
        kernel: std::path::PathBuf,

        /// Directory to create/update with runtime artifacts.
        #[arg(long)]
        out_dir: std::path::PathBuf,

        /// Exported kernel symbol naming the first ring byte.
        #[arg(long, default_value = "TX_OBSERVE_RINGS")]
        symbol: String,

        /// Number of per-hart rings to drain.
        #[arg(long, default_value_t = 4)]
        hart_count: usize,

        /// Bytes reserved per hart ring in the guest.
        #[arg(long, default_value_t = 2 * 1024 * 1024)]
        ring_bytes: usize,

        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 2)]
        poll_ms: u64,

        /// Stop after this file appears and one quiet poll drains no records.
        #[arg(long)]
        stop_file: Option<std::path::PathBuf>,

        /// Optional hard stop for bounded tests.
        #[arg(long)]
        max_duration_ms: Option<u64>,

        /// Also materialize replay.ndjson and trace.pftrace after raw drain.
        #[arg(long)]
        finalize: bool,

        /// Optional path to a `names.json` file for EventNameId resolution.
        #[arg(long)]
        names: Option<std::path::PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Replay {
            file,
            out,
            output,
            filter,
            names,
        } => match out.as_str() {
            "json" => {
                let min_level = parse_filter(filter.as_deref());
                replay::run(&file, min_level)
            }
            "pftrace" => {
                let out_path = output.unwrap_or_else(|| {
                    let mut p = file.clone();
                    p.set_extension("pftrace");
                    p
                });
                let names_map = names.as_deref().and_then(|p| load_names_json(p));
                replay::run_pftrace(&file, &out_path, names_map)
            }
            other => {
                eprintln!("error: unknown --out value '{other}'; supported: json, pftrace");
                std::process::exit(1);
            }
        },
        Command::Bundle {
            file,
            out_dir,
            names,
        } => {
            let names_map = names.as_deref().and_then(|p| load_names_json(p));
            match replay::write_bundle(&file, &out_dir, names_map, names.as_deref()) {
                Ok(stats) => {
                    eprintln!(
                        "bundle: {} records, {} lost, {} framing errors, complete={}",
                        stats.total_records, stats.total_lost, stats.framing_errors, stats.complete,
                    );
                    Ok(())
                }
                Err(err) => Err(err),
            }
        }
        Command::LiveGuestMem {
            guest_mem,
            kernel,
            out_dir,
            symbol,
            hart_count,
            ring_bytes,
            poll_ms,
            stop_file,
            max_duration_ms,
            finalize,
            names,
        } => {
            let names_map = names.as_deref().and_then(|p| load_names_json(p));
            let config = replay::LiveDrainConfig {
                guest_mem: &guest_mem,
                kernel: &kernel,
                out_dir: &out_dir,
                symbol: &symbol,
                hart_count,
                ring_bytes,
                stop_file: stop_file.as_deref(),
                poll_ms,
                max_duration_ms,
                finalize,
                names_map,
            };
            match replay::run_live_guest_mem(config) {
                Ok(stats) => {
                    eprintln!(
                        "live-guest-mem: {} harts, {} visible, {} lost, complete={}",
                        stats.hart_count, stats.total_records, stats.total_lost, stats.complete,
                    );
                    Ok(())
                }
                Err(err) => Err(err),
            }
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Parse `--filter level=N` into an optional minimum level byte.
fn parse_filter(filter: Option<&str>) -> Option<u8> {
    let s = filter?;
    let rest = s.strip_prefix("level=")?;
    rest.parse::<u8>().ok()
}

/// Load a names.json file and return the `name_table` as a `HashMap<u32, String>`.
fn load_names_json(path: &std::path::Path) -> Option<std::collections::HashMap<u32, String>> {
    let data = std::fs::read_to_string(path)
        .map_err(|e| eprintln!("warning: failed to read names file {}: {e}", path.display()))
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&data)
        .map_err(|e| eprintln!("warning: failed to parse names file: {e}"))
        .ok()?;
    let table = v.get("name_table")?.as_object()?;
    let mut map = std::collections::HashMap::new();
    for (k, v) in table {
        if let (Ok(id), Some(name)) = (k.parse::<u32>(), v.as_str()) {
            map.insert(id, name.to_string());
        }
    }
    Some(map)
}
