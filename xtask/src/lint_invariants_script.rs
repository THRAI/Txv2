//! Lint rule for SCRIPT-2: Scripts must not import subsystem `structure/`.
//!
//! Per `docs/Txv3/02_INVARIANTS_v5.md` SCRIPT-*: "Scripts own sequencing;
//! do not own truth; do not inspect subsystem structure/; do not store
//! witnesses; do not define authoritative indexes."
//!
//! Per `MODULE_MAP_v1.md` §13: forbidden imports include "scripts importing
//! subsystem structure/."

use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Ratchet ceiling: number of script files importing `tx_subsystems::*::structure`.
/// Will be set to measured baseline.
const MAX_SCRIPT_STRUCTURE_IMPORTS: usize = 5;

pub(crate) fn lint_invariants_script_boundary(root: &Path) -> Result<()> {
    let scripts_path = root.join("crates/tx-scripts/src");
    if !scripts_path.exists() {
        return Ok(());
    }

    let files = collect_files(&scripts_path, &["rs"]).map_err(|e| e.to_string())?;

    let mut violations: Vec<String> = Vec::new();

    for file in &files {
        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let rel = relative(root, file).replace('\\', "/");

        for (line_num, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            // Skip comments
            if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                continue;
            }

            // Match: line contains both `tx_subsystems` AND `::structure`
            if trimmed.contains("tx_subsystems") && trimmed.contains("::structure") {
                violations.push(format!("{rel}:{} — {}", line_num + 1, trimmed));
            }
        }
    }

    let count = violations.len();

    println!("Invariants Lint — script-boundary (SCRIPT-2)");
    println!("=============================================");

    let status = if count > MAX_SCRIPT_STRUCTURE_IMPORTS {
        "OVER"
    } else {
        "ok"
    };
    println!(
        "script→structure imports: {:>4}  (ceiling {})  {}",
        count, MAX_SCRIPT_STRUCTURE_IMPORTS, status
    );

    for v in &violations {
        println!("  {v}");
    }

    if count > MAX_SCRIPT_STRUCTURE_IMPORTS {
        return Err(format!(
            "script-boundary ratchet regression — {count} structure imports > ceiling {MAX_SCRIPT_STRUCTURE_IMPORTS}"
        ));
    }

    Ok(())
}
