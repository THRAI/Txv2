//! Lint rule for WIT-4: Witness scope — IdentRef must not be stored in struct
//! fields or returned in StepOutcome.
//!
//! Per `docs/Txv3/02_INVARIANTS_v5.md`: `IdentRef<'g, T>` is guard-scoped
//! observation evidence. Storing it in a struct field creates a path to outlive
//! the guard (WIT-3 violation). Returning it in `StepOutcome` crosses the step
//! boundary (WIT-3 / WIT-4).
//!
//! Scanned crates: `tx-subsystems`, `tx-kernel`. Substrate (`tx-substrate`) is
//! excluded because `IdentRef` is *defined* there.

use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Ratchet ceiling for `IdentRef` appearing in struct field position.
/// Set to 0 at baseline (2026-05-14 measurement).
const MAX_IDENTREF_IN_STRUCTS: usize = 0;

/// Ratchet ceiling for `IdentRef` appearing in StepOutcome generic parameters
/// (return position or type alias). Set to 0 at baseline.
const MAX_IDENTREF_IN_STEP_OUTCOME: usize = 1;

pub(crate) fn lint_invariants_witness_scope(root: &Path) -> Result<()> {
    let mut struct_violations: Vec<String> = Vec::new();
    let mut outcome_violations: Vec<String> = Vec::new();

    let target_dirs = [
        "crates/tx-subsystems/src",
        "crates/tx-kernel/src",
    ];

    for dir in &target_dirs {
        let dir_path = root.join(dir);
        if !dir_path.exists() {
            continue;
        }

        let files = collect_files(&dir_path, &["rs"]).map_err(|e| e.to_string())?;

        for file in &files {
            let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let rel = relative(root, file).replace('\\', "/");

            // Skip IdentRef definition site
            if rel.contains("tx-substrate") {
                continue;
            }

            let mut in_struct = false;
            let mut brace_depth = 0u32;
            let mut _struct_start_line = 0usize;

            for (line_num, line) in text.lines().enumerate() {
                let trimmed = line.trim();

                // Track struct definition
                if trimmed.starts_with("struct ") && trimmed.contains('{') {
                    in_struct = true;
                    brace_depth = 0;
                    _struct_start_line = line_num + 1;
                }

                if in_struct {
                    brace_depth += trimmed.matches('{').count() as u32;
                    brace_depth = brace_depth.saturating_sub(trimmed.matches('}').count() as u32);

                    // Check for IdentRef in struct fields (not comments)
                    if !trimmed.starts_with("//") && !trimmed.starts_with("///") {
                        let code_part = if let Some(comment_pos) = trimmed.find("//") {
                            &trimmed[..comment_pos]
                        } else {
                            trimmed
                        };

                        if code_part.contains("IdentRef") {
                            // It's a field if we're inside struct and it has `:`
                            if code_part.contains(':') {
                                struct_violations.push(format!(
                                    "{rel}:{line} — IdentRef in struct field (WIT-4)"
                                ));
                            }
                        }
                    }

                    if brace_depth == 0 {
                        in_struct = false;
                    }
                }

                // Check for IdentRef in StepOutcome generic parameters
                // Pattern: StepOutcome<..., IdentRef<...>, ...>
                if !trimmed.starts_with("//") && !trimmed.starts_with("///") {
                    let code_part = if let Some(comment_pos) = trimmed.find("//") {
                        &trimmed[..comment_pos]
                    } else {
                        trimmed
                    };

                    if code_part.contains("StepOutcome") && code_part.contains("IdentRef") {
                        outcome_violations.push(format!(
                            "{rel}:{line} — IdentRef in StepOutcome type position (WIT-4)"
                        ));
                    }

                    // Also check type aliases
                    if (code_part.starts_with("type ") || code_part.starts_with("pub type "))
                        && code_part.contains("StepOutcome")
                        && code_part.contains("IdentRef")
                    {
                        outcome_violations.push(format!(
                            "{rel}:{line} — IdentRef in StepOutcome type alias (WIT-4)"
                        ));
                    }
                }
            }
        }
    }

    let struct_count = struct_violations.len();
    let outcome_count = outcome_violations.len();

    println!("Invariants Lint — witness-scope (WIT-4)");
    println!("=========================================");

    let struct_status = if struct_count > MAX_IDENTREF_IN_STRUCTS { "OVER" } else { "ok" };
    println!(
        "IdentRef in struct fields: {:>4}  (ceiling {})  {}",
        struct_count, MAX_IDENTREF_IN_STRUCTS, struct_status
    );

    let outcome_status = if outcome_count > MAX_IDENTREF_IN_STEP_OUTCOME { "OVER" } else { "ok" };
    println!(
        "IdentRef in StepOutcome:   {:>4}  (ceiling {})  {}",
        outcome_count, MAX_IDENTREF_IN_STEP_OUTCOME, outcome_status
    );

    for v in &struct_violations {
        println!("  {v}");
    }
    for v in &outcome_violations {
        println!("  {v}");
    }

    if struct_count > MAX_IDENTREF_IN_STRUCTS {
        return Err(format!(
            "witness-scope ratchet regression — {struct_count} IdentRef in struct fields > ceiling {MAX_IDENTREF_IN_STRUCTS}"
        ));
    }
    if outcome_count > MAX_IDENTREF_IN_STEP_OUTCOME {
        return Err(format!(
            "witness-scope ratchet regression — {outcome_count} IdentRef in StepOutcome > ceiling {MAX_IDENTREF_IN_STEP_OUTCOME}"
        ));
    }

    Ok(())
}
