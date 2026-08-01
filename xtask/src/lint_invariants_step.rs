//! Lint rule for STEP-4: Five-stage step discipline.
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` §4: every mutating step must follow
//! observe → upgrade → reserve → commit → publish. This lint checks that
//! `pub fn step_*` functions in `crates/tx-subsystems/src/` have inline
//! comments marking each stage.

use std::fs;
use std::path::Path;

use crate::Result;
use crate::util::{collect_files, relative};

/// Ratchet ceiling: number of step functions missing one or more stage comments.
/// Will be set to measured baseline after first run.
const MAX_STEPS_WITHOUT_5_STAGE: usize = 5; // 0→1: RenameOp in composite.rs (legitimate pass-through wrapper)

const STAGE_MARKERS: &[&str] = &["observe", "upgrade", "reserve", "commit", "publish"];

/// Count step functions that are missing stage comments.
pub(crate) fn lint_invariants_step_discipline(root: &Path) -> Result<()> {
    let subsys_path = root.join("crates/tx-subsystems/src");
    if !subsys_path.exists() {
        return Ok(());
    }

    let files = collect_files(&subsys_path, &["rs"]).map_err(|e| e.to_string())?;

    let mut total_steps = 0usize;
    let mut missing_stages: Vec<String> = Vec::new();

    for file in &files {
        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let rel = relative(root, file).replace('\\', "/");

        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0usize;

        while i < lines.len() {
            let line = lines[i].trim();

            // Match `pub fn step_*` function definitions
            if (line.starts_with("pub fn step_") || line.starts_with("pub(crate) fn step_"))
                && line.contains('(')
            {
                let fn_name = line
                    .split("fn ")
                    .nth(1)
                    .and_then(|s| s.split('(').next())
                    .unwrap_or("step_*");

                total_steps += 1;

                // Scan fn body for stage markers
                let mut found_stages: usize = 0;
                let mut depth = 0u32;
                let mut in_fn = false;
                let mut j = i;

                while j < lines.len() {
                    let body_line = lines[j];
                    let trimmed_body = body_line.trim().to_lowercase();

                    let opens = body_line.matches('{').count() as u32;
                    let closes = body_line.matches('}').count() as u32;

                    if opens > 0 && !in_fn {
                        in_fn = true;
                        depth = opens.saturating_sub(closes);
                        j += 1;
                        continue;
                    }

                    if in_fn {
                        depth += opens;
                        depth = depth.saturating_sub(closes);

                        for marker in STAGE_MARKERS {
                            // Accept: // observe, //observe, // ① observe, // observe:, // observe —, etc.
                            let is_comment_line = trimmed_body.starts_with("//");
                            if is_comment_line && trimmed_body.contains(marker) {
                                found_stages |=
                                    1 << STAGE_MARKERS.iter().position(|m| m == marker).unwrap();
                            }
                        }

                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }

                // All five stages required: bits 0-4 must all be set
                if found_stages != 0b11111 {
                    let missing: Vec<&str> = STAGE_MARKERS
                        .iter()
                        .enumerate()
                        .filter(|(idx, _)| found_stages & (1 << idx) == 0)
                        .map(|(_, m)| *m)
                        .collect();
                    missing_stages
                        .push(format!("{rel}:{fn_name} — missing: {}", missing.join(", ")));
                }

                i = j;
            }
            i += 1;
        }
    }

    let missing_count = missing_stages.len();

    println!("Invariants Lint — step-discipline (STEP-4)");
    println!("============================================");
    println!("total step functions found: {total_steps}");

    let status = if missing_count > MAX_STEPS_WITHOUT_5_STAGE {
        "OVER"
    } else {
        "ok"
    };
    println!(
        "missing ≥1 stage comment: {missing_count:>4}  (ceiling {MAX_STEPS_WITHOUT_5_STAGE})  {status}"
    );

    for m in &missing_stages {
        println!("  {m}");
    }

    if missing_count > MAX_STEPS_WITHOUT_5_STAGE {
        return Err(format!(
            "step-discipline ratchet regression — {missing_count} steps missing stages > ceiling {MAX_STEPS_WITHOUT_5_STAGE}"
        ));
    }

    Ok(())
}
