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
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Replay { file, out, output, filter, names } => match out.as_str() {
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
