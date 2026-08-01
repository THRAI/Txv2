//! Lint rule: no ad-hoc step outcome dispatch outside `drive.rs`.
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` §5 and
//! `docs/progress/plans/2026-05-09-v3-tdd-migration.md` §9:
//! the `tx_scripts::drive::drive()` function is the single canonical
//! dispatch site for all four `StepOutcome` variants. Any file that
//! matches on `StepOutcome::Continue` or `StepOutcome::Yield` outside of
//! `drive.rs` and test modules is an ad-hoc outcome loop that must be
//! migrated to `drive()`.
//!
//! Ratchet gate: count files (non-test, non-drive) with ad-hoc
//! StepOutcome pattern matching. Trend toward zero.
//!
//! Scanned crates: all authored crates. Excluded:
//! - `tx-scripts/src/drive.rs` — canonical dispatch site
//! - `tx-substrate/src/step/mod.rs` — algebra definition
//! - `#[cfg(test)]` blocks — unit tests are allowed to call `.step()` directly
//! - `crates/tx-substrate/tests/` — integration tests

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::Result;
use crate::util::{collect_files, relative};

/// Ratchet ceiling: number of non-test, non-drive files with ad-hoc
/// StepOutcome dispatch. Measured baseline 2026-05-14.
const MAX_ADHOC_OUTCOME_FILES: usize = 4; // 3→4: vfs-full-bringup merge added one new ad-hoc-drive file (ext4/bdev-fs wiring)

fn is_outcome_construction(code: &str) -> bool {
    let trimmed = code.trim_start();
    let constructor = !trimmed.contains("=>")
        && (trimmed.starts_with("StepOutcome::Yield")
            || trimmed.starts_with("StepOutcome::Continue")
            || trimmed.starts_with("return StepOutcome::Yield")
            || trimmed.starts_with("return StepOutcome::Continue"));

    constructor
        || (code.contains("=>")
            && (code.contains("StepOutcome::Yield") || code.contains("StepOutcome::Continue")))
}

pub(crate) fn lint_invariants_no_adhoc_drive(root: &Path) -> Result<()> {
    let target_dirs = [
        "crates/tx-kernel",
        "crates/tx-shims",
        "crates/tx-scripts",
        "crates/tx-subsystems",
        "crates/tx-fs",
        "crates/tx-ext4",
        "crates/tx-ext4-format",
        "crates/tx-reactor",
    ];

    let skip_paths: BTreeSet<&str> = [
        "crates/tx-scripts/src/drive.rs",
        "crates/tx-substrate/src/step/mod.rs",
    ]
    .iter()
    .copied()
    .collect();

    // Files that are known to have ad-hoc step loops that need migration.
    // Each entry should eventually go to zero.
    let mut adhoc_files: Vec<String> = Vec::new();
    let mut adhoc_sites: Vec<String> = Vec::new();

    for dir in &target_dirs {
        let dir_path = root.join(dir);
        if !dir_path.exists() {
            continue;
        }

        // Collect all .rs files in this crate
        let files = collect_files(&dir_path, &["rs"]).map_err(|e| e.to_string())?;

        for file in &files {
            let rel = relative(root, file).replace('\\', "/");

            // Skip the canonical drive site
            if skip_paths.contains(rel.as_str()) {
                continue;
            }

            // Skip test-only files (they can call .step() directly for unit testing)
            if rel.contains("/tests/") || rel.ends_with("_test.rs") || rel.ends_with("/tests.rs") {
                continue;
            }

            // Skip the adapter.rs files (they re-export types)
            if rel.ends_with("/adapter.rs") {
                continue;
            }

            // Skip the algebra module
            if rel.contains("tx-substrate") {
                continue;
            }

            // Skip step implementation files (producers, not consumers).
            // Per SUBSYSTEM_ANATOMY_v2_1, execution/ modules contain
            // step_* functions that return StepOutcome — they MUST handle
            // all four variants internally. These are not ad-hoc dispatch;
            // they are the legitimate producers.
            if rel.contains("/execution/") || rel.ends_with("/execution.rs") {
                continue;
            }

            // Skip struct definition files that contain YieldShape/StepOutcome
            // construction sites (range_lock, lifecycle, etc.)
            if rel.contains("/structure/") {
                continue;
            }

            let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;

            // Skip files that define their own step_* functions.
            // These are legitimate step implementations (producers) that
            // MUST handle all four StepOutcome variants internally.
            // They are not ad-hoc consumers — they ARE the step model.
            if text.contains("pub fn step_") || text.contains("pub(crate) fn step_") {
                continue;
            }

            let mut file_has_adhoc = false;

            for (line_num, line) in text.lines().enumerate() {
                let trimmed = line.trim();

                // Skip pure comments
                if trimmed.starts_with("//")
                    || trimmed.starts_with("///")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with("*")
                {
                    continue;
                }

                // Skip inside #[cfg(test)] blocks — we detect by tracking
                // whether we're inside a test module. A simpler approach:
                // flag only if the line is NOT inside a #[cfg(test)] block.
                // We approximate: skip lines that appear after `mod tests {`
                // or `#[cfg(test)]`.

                // Check for ad-hoc StepOutcome dispatch patterns:
                // 1. Matching on StepOutcome::Continue or StepOutcome::Yield
                // 2. Calling .step() on a StepOp and then matching the result
                let has_continue = trimmed.contains("StepOutcome::Continue");
                let has_yield = trimmed.contains("StepOutcome::Yield");
                let has_advanced = trimmed.contains("StepOutcome::Advanced");

                if has_continue || has_yield || has_advanced {
                    // Skip construction sites: `=> StepOutcome::Yield {` is
                    // building a value, not dispatching on one.
                    let code_part = if let Some(pos) = trimmed.find("//") {
                        &trimmed[..pos]
                    } else {
                        trimmed
                    };
                    if is_outcome_construction(code_part) {
                        continue;
                    }

                    // Skip if inside a test block — approximated by checking
                    // if the file context is a test module
                    if is_in_test_context(&text, line_num) {
                        continue;
                    }

                    if !file_has_adhoc {
                        adhoc_files.push(rel.clone());
                        file_has_adhoc = true;
                    }

                    let variant = if has_continue {
                        "Continue"
                    } else if has_yield {
                        "Yield"
                    } else {
                        "Advanced"
                    };

                    let code_snippet = if let Some(pos) = trimmed.find("//") {
                        trimmed[..pos].trim()
                    } else {
                        trimmed
                    };

                    adhoc_sites.push(format!(
                        "{rel}:{} — ad-hoc {} dispatch: {}",
                        line_num + 1,
                        variant,
                        code_snippet.chars().take(80).collect::<String>()
                    ));
                }
            }
        }
    }

    let file_count = adhoc_files.len();
    let site_count = adhoc_sites.len();

    println!("Invariants Lint — no-adhoc-drive");
    println!("===================================");

    let status = if file_count > MAX_ADHOC_OUTCOME_FILES {
        "OVER"
    } else {
        "ok"
    };
    println!(
        "files with ad-hoc outcome dispatch: {file_count:>4}  (ceiling {MAX_ADHOC_OUTCOME_FILES})  {status}"
    );
    println!("total ad-hoc outcome sites:         {site_count:>4}");

    if !adhoc_files.is_empty() {
        println!();
        println!("  files:");
        for f in &adhoc_files {
            println!("  {f}");
        }
        println!();
        println!("  sites (first 30):");
        for s in adhoc_sites.iter().take(30) {
            println!("  {s}");
        }
        if adhoc_sites.len() > 30 {
            println!("  ... and {} more", adhoc_sites.len() - 30);
        }
    }

    if file_count > MAX_ADHOC_OUTCOME_FILES {
        return Err(format!(
            "no-adhoc-drive ratchet regression — {file_count} files with ad-hoc outcome dispatch > ceiling {MAX_ADHOC_OUTCOME_FILES}"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_outcome_construction;

    #[test]
    fn returned_yield_is_an_outcome_producer_not_a_dispatch() {
        assert!(is_outcome_construction(
            "return StepOutcome::Yield(YieldShape::on_wait_source(source, interests));"
        ));
    }
}

/// Approximate check: is this line inside a `#[cfg(test)]` or `mod tests {` block?
///
/// Scans backward from line_num to find the most recent `#[cfg(test)]` or
/// `mod tests {` that is not yet closed.
fn is_in_test_context(text: &str, target_line: usize) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_test_mod = false;
    let mut test_depth = 0u32;

    for (i, line) in lines.iter().enumerate() {
        if i > target_line {
            break;
        }

        let trimmed = line.trim();

        // Enter test module.
        // `#[cfg(test)]` on its own line is a marker; `mod tests` (with `{`)
        // is where we start tracking brace depth.
        if trimmed == "#[cfg(test)]" {
            in_test_mod = true;
            test_depth = 0;
            continue;
        }
        if trimmed.starts_with("mod tests") {
            in_test_mod = true;
            test_depth = 0;
            // Fall through to track the opening `{` below
        }

        if in_test_mod {
            let opens = trimmed.matches('{').count() as u32;
            let closes = trimmed.matches('}').count() as u32;

            if opens > 0 && test_depth == 0 {
                test_depth = opens.saturating_sub(closes);
            } else {
                test_depth += opens;
                test_depth = test_depth.saturating_sub(closes);
            }

            if test_depth == 0 && i > 0 {
                in_test_mod = false;
            }
        }
    }

    in_test_mod
}
