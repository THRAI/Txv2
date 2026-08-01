//! Lint rule for PRED-1: Checks/ purity — no mutation calls in checks files.
//!
//! Per `docs/Txv3/02_INVARIANTS_v5.md` PRED-1: "This predicate mutates or
//! blocks; checks must stay pure."
//!
//! Per `SUBSYSTEM_ANATOMY_v2_1.md` §2: checks/ module does "observation +
//! witness production, no mutation."

use std::fs;
use std::path::Path;

use crate::Result;
use crate::util::{collect_files, relative};

/// Ratchet ceiling: number of mutation calls found in checks/ files.
const MAX_MUTATIONS_IN_CHECKS: usize = 0;

/// Mutation markers — substrate APIs that mutate state.
const MUTATION_MARKERS: &[&str] = &[
    "zone::sign(",
    "zone::reserve(",
    "index::install(",
    "bus::",
    "mutation::",
    "_commit(",
    "withdraw(",
    "swap(",
];

pub(crate) fn lint_invariants_checks_purity(root: &Path) -> Result<()> {
    let subsys_path = root.join("crates/tx-subsystems/src");
    if !subsys_path.exists() {
        return Ok(());
    }

    let all_files = collect_files(&subsys_path, &["rs"]).map_err(|e| e.to_string())?;

    // Filter to checks/ files only
    let checks_files: Vec<_> = all_files
        .into_iter()
        .filter(|f| {
            let path_str = f.to_string_lossy().replace('\\', "/");
            path_str.contains("/checks/") || path_str.ends_with("/checks.rs")
        })
        .collect();

    let mut violations: Vec<String> = Vec::new();

    for file in &checks_files {
        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let rel = relative(root, file).replace('\\', "/");

        for (line_num, line) in text.lines().enumerate() {
            // Skip pure comment lines
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with("*") {
                continue;
            }

            // Strip end-of-line comments
            let code = if let Some(pos) = trimmed.find("//") {
                &trimmed[..pos]
            } else {
                trimmed
            };

            for marker in MUTATION_MARKERS {
                if code.contains(marker) {
                    violations.push(format!(
                        "{rel}:{} — mutation: {}  [PRED-1]",
                        line_num + 1,
                        marker
                    ));
                    break; // one violation per line
                }
            }
        }
    }

    let count = violations.len();

    println!("Invariants Lint — checks-purity (PRED-1)");
    println!("=========================================");

    let status = if count > MAX_MUTATIONS_IN_CHECKS {
        "OVER"
    } else {
        "ok"
    };
    println!(
        "mutation calls in checks/ files: {count:>4}  (ceiling {MAX_MUTATIONS_IN_CHECKS})  {status}"
    );

    for v in &violations {
        println!("  {v}");
    }

    if count > MAX_MUTATIONS_IN_CHECKS {
        return Err(format!(
            "checks-purity ratchet regression — {count} mutations > ceiling {MAX_MUTATIONS_IN_CHECKS}"
        ));
    }

    Ok(())
}
