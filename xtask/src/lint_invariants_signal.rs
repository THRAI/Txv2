//! Lint rule for SIG-4 and SIG-6: signal publication ordering.
//!
//! Per `docs/Txv3/02_INVARIANTS_v5.md`:
//! - SIG-4: Always fire after `substrate::*_commit` returns.
//! - SIG-6: Signals only from publish stage.
//!
//! Checks that bus `publish` / `fire` / `push` calls appear AFTER commit calls
//! in step functions within `crates/tx-subsystems/src/`.

use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Ratchet ceiling: number of step functions with publish-before-commit ordering.
/// Will be set to measured baseline.
const MAX_SIGNAL_BEFORE_COMMIT: usize = 0;

/// Commit-like markers in step function bodies.
const COMMIT_MARKERS: &[&str] = &["_commit(", "sign(", "install_", "withdraw(", "swap("];

/// Publish-like markers in step function bodies.
const PUBLISH_MARKERS: &[&str] = &["publish(", "fire(", ".push(", "enqueue("];

pub(crate) fn lint_invariants_signal_publish(root: &Path) -> Result<()> {
    let mut violations: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let subsys_path = root.join("crates/tx-subsystems/src");
    if !subsys_path.exists() {
        return Ok(());
    }

    let files = collect_files(&subsys_path, &["rs"]).map_err(|e| e.to_string())?;

    for file in &files {
        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let rel = relative(root, file).replace('\\', "/");

        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0usize;

        while i < lines.len() {
            let line = lines[i].trim();

            // Find step function definitions
            if line.starts_with("pub fn step_") || line.starts_with("pub(crate) fn step_") {
                let fn_name = line
                    .split("fn ")
                    .nth(1)
                    .and_then(|s| s.split('(').next())
                    .unwrap_or("step_*");

                // Scan the function body for commit and publish lines
                let mut depth = 0u32;
                let mut in_fn = false;
                let mut commit_lines: Vec<usize> = Vec::new();
                let mut publish_lines: Vec<usize> = Vec::new();
                let mut j = i;

                while j < lines.len() {
                    let body_line = lines[j];

                    // Track brace depth
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

                        // Skip comments
                        let code = if let Some(pos) = body_line.find("//") {
                            &body_line[..pos]
                        } else {
                            body_line
                        };

                        // Check for commit markers
                        for marker in COMMIT_MARKERS {
                            if code.contains(marker) {
                                commit_lines.push(j + 1); // 1-based line number
                            }
                        }

                        // Check for publish markers
                        for marker in PUBLISH_MARKERS {
                            if code.contains(marker) {
                                publish_lines.push(j + 1);
                            }
                        }

                        if depth == 0 {
                            break;
                        }
                    }

                    j += 1;
                }

                // Check ordering
                if !commit_lines.is_empty() && !publish_lines.is_empty() {
                    let max_commit = *commit_lines.iter().max().unwrap();
                    let min_publish = *publish_lines.iter().min().unwrap();
                    if min_publish < max_commit {
                        violations.push(format!(
                            "{rel}:{fn_name} — publish (line {min_publish}) before commit (line {max_commit})  [SIG-4]",
                        ));
                    }
                } else if !commit_lines.is_empty() && publish_lines.is_empty() {
                    warnings.push(format!(
                        "{rel}:{fn_name} — commit but no signal publish  [SIG-3 cross-check]"
                    ));
                }

                i = j;
            }

            i += 1;
        }
    }

    let violation_count = violations.len();

    println!("Invariants Lint — signal-publish (SIG-4, SIG-6)");
    println!("==================================================");

    let status = if violation_count > MAX_SIGNAL_BEFORE_COMMIT {
        "OVER"
    } else {
        "ok"
    };
    println!(
        "violations (publish-before-commit): {violation_count:>4}  (ceiling {MAX_SIGNAL_BEFORE_COMMIT})  {status}"
    );

    for v in &violations {
        println!("  {v}");
    }
    if !warnings.is_empty() {
        println!("\nwarnings (commit without publish — cross-check SIGNAL_ATTACHMENTS_v1.md):");
        for w in &warnings {
            println!("  {w}");
        }
    }

    if violation_count > MAX_SIGNAL_BEFORE_COMMIT {
        return Err(format!(
            "signal-publish ratchet regression — {violation_count} violations > ceiling {MAX_SIGNAL_BEFORE_COMMIT}. \
             Each violation names a step where publish precedes commit (SIG-4)."
        ));
    }

    Ok(())
}
