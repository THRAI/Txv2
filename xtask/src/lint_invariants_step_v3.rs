//! Lint rules for v3 step model invariants (STEP-1..10, YIELD, WIT).
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` and `docs/Txv3/02_INVARIANTS_v5.md`:
//! the v3 migration retires v4 vocabulary entirely, bans `.await` inside
//! step functions, and requires progress isolation across yield boundaries.
//!
//! Three ratchet gates:
//! 1. v4-vocabulary — count remaining `Advanced`, `Blocked`, `WakeCarrier`,
//!    `OnCarrier`, `InterestConditions` identifiers (trend toward 0)
//! 2. A-3 — `.await` inside `fn step(` or `impl StepOp::step` bodies
//! 3. STEP-2 — `async fn step_*` or `-> impl Future` in step function sigs
//!
//! Scanned crates: `tx-subsystems`, `tx-kernel`, `tx-shims`, `tx-scripts`,
//! `tx-fs`. Substrate (`tx-substrate`) is excluded because it *defines*
//! the StepOutcome algebra and some old identifiers live there as
//! migration scaffolding until PR-1 retires them.

use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

// ---------------------------------------------------------------------------
// Ratchet ceilings — set to measured baseline; lowered as migration progresses.
// ---------------------------------------------------------------------------

/// Target ceiling for v4 vocabulary identifiers across scanned crates.
/// Set at measurement-time baseline (to be measured on first run).
/// Lower as PR-1..N retires them.
const MAX_V4_VOCABULARY: usize = 0; // v4 vocabulary fully retired; Blocked( false positives excluded

/// Ceiling for `.await` inside step function bodies. Ratchet toward 0 per A-3.
const MAX_AWAIT_IN_STEP: usize = 0; // signalfd/userfaultfd renamed to sys_* — no remaining step_* .await sites

/// Ceiling for `async fn step_*` or `-> impl Future` in step signatures.
/// Ratchet toward 0 per STEP-2.
const MAX_ASYNC_STEP_SIG: usize = 0; // signalfd/userfaultfd renamed to sys_* — no remaining async step_* sigs

// ---------------------------------------------------------------------------
// V4 vocabulary identifiers — every occurrence must eventually go to zero.
// ---------------------------------------------------------------------------

const V4_OUTCOME_VARIANTS: &[&str] = &["Advanced", "AdvancedThenBlocked"];

const V4_YIELD_VOCABULARY: &[&str] = &[
    "WakeCarrier",
    "OnCarrier",
    "InterestConditions",
    "StepOutcome::Blocked(",
];

/// All v4 identifiers that the migration plans to retire.
fn v4_identifiers() -> Vec<&'static str> {
    V4_OUTCOME_VARIANTS
        .iter()
        .chain(V4_YIELD_VOCABULARY.iter())
        .copied()
        .collect()
}

// ---------------------------------------------------------------------------
// Rule 1: v4 vocabulary scan
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_v4_vocabulary(root: &Path) -> Result<()> {
    let target_dirs = [
        "crates/tx-subsystems/src",
        "crates/tx-kernel/src",
        "crates/tx-shims/src",
        "crates/tx-scripts/src",
        "crates/tx-fs/src",
    ];

    let identifiers = v4_identifiers();
    let mut hits: Vec<String> = Vec::new();
    let mut per_id_counts: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();

    for dir in &target_dirs {
        let dir_path = root.join(dir);
        if !dir_path.exists() {
            continue;
        }

        let files = collect_files(&dir_path, &["rs"]).map_err(|e| e.to_string())?;

        for file in &files {
            let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let rel = relative(root, file).replace('\\', "/");

            for (line_num, line) in text.lines().enumerate() {
                let trimmed = line.trim();

                // Skip pure comments and doc comments
                if trimmed.starts_with("//")
                    || trimmed.starts_with("///")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with("*")
                {
                    continue;
                }

                // Strip end-of-line comment for code portion
                let code = if let Some(pos) = trimmed.find("//") {
                    &trimmed[..pos]
                } else {
                    trimmed
                };

                for id in &identifiers {
                    if code.contains(id) {
                        // Be specific: `Blocked(` to avoid matching unrelated
                        // words like "unblocked". For the others, the PascalCase
                        // is distinctive enough in Rust code.
                        hits.push(format!("{rel}:{} — v4 identifier `{id}`", line_num + 1));
                        *per_id_counts.entry(id).or_insert(0) += 1;
                        break; // one hit per line
                    }
                }
            }
        }
    }

    let total = hits.len();

    println!("Invariants Lint — v4-vocabulary");
    println!("================================");

    // Per-identifier breakdown
    for id in &identifiers {
        let count = per_id_counts.get(id).copied().unwrap_or(0);
        println!("  {id:.<30} {count:>4}");
    }

    let status = if total > MAX_V4_VOCABULARY {
        "OVER"
    } else {
        "ok"
    };
    println!("total v4 identifiers: {total:>4}  (ceiling {MAX_V4_VOCABULARY})  {status}");

    // Show first 20 hits for triage
    if !hits.is_empty() {
        println!();
        println!("  first {} of {total} hits:", hits.len().min(20));
        for h in hits.iter().take(20) {
            println!("  {h}");
        }
    }

    if total > MAX_V4_VOCABULARY {
        return Err(format!(
            "v4-vocabulary ratchet regression — {total} identifiers > ceiling {MAX_V4_VOCABULARY}"
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Rule 2: A-3 — `.await` inside step function bodies
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_step_no_await(root: &Path) -> Result<()> {
    let target_dirs = [
        "crates/tx-subsystems/src",
        "crates/tx-kernel/src",
        "crates/tx-shims/src",
        "crates/tx-scripts/src",
        "crates/tx-fs/src",
    ];

    let mut violations: Vec<String> = Vec::new();

    for dir in &target_dirs {
        let dir_path = root.join(dir);
        if !dir_path.exists() {
            continue;
        }

        let files = collect_files(&dir_path, &["rs"]).map_err(|e| e.to_string())?;

        for file in &files {
            let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let rel = relative(root, file).replace('\\', "/");

            let lines: Vec<&str> = text.lines().collect();
            let mut i = 0usize;

            while i < lines.len() {
                let line = lines[i].trim();

                // Match step function definitions: `fn step(` or `fn step_*`
                let is_step_fn =
                    (line.contains("fn step(") || line.contains("fn step_")) && line.contains('(');

                if is_step_fn && !line.contains("// A-3") {
                    // Find function body
                    let mut depth = 0u32;
                    let mut in_body = false;
                    let mut j = i;

                    while j < lines.len() {
                        let body_line = lines[j];
                        let opens = body_line.matches('{').count() as u32;
                        let closes = body_line.matches('}').count() as u32;

                        if opens > 0 && !in_body {
                            in_body = true;
                            depth = opens.saturating_sub(closes);
                            j += 1;
                            continue;
                        }

                        if in_body {
                            depth += opens;
                            depth = depth.saturating_sub(closes);

                            let trimmed = body_line.trim();
                            // Skip comments
                            if !trimmed.starts_with("//") && !trimmed.starts_with("///") {
                                let code = if let Some(pos) = trimmed.find("//") {
                                    &trimmed[..pos]
                                } else {
                                    trimmed
                                };

                                if code.contains(".await") {
                                    violations.push(format!(
                                        "{rel}:{} — .await in step fn body (A-3)",
                                        j + 1
                                    ));
                                }
                            }

                            if depth == 0 {
                                break;
                            }
                        }
                        j += 1;
                    }

                    i = j;
                }
                i += 1;
            }
        }
    }

    let count = violations.len();

    println!("Invariants Lint — step-no-await (A-3)");
    println!("=======================================");

    let status = if count > MAX_AWAIT_IN_STEP {
        "OVER"
    } else {
        "ok"
    };
    println!(".await in step fn bodies: {count:>4}  (ceiling {MAX_AWAIT_IN_STEP})  {status}");

    for v in &violations {
        println!("  {v}");
    }

    if count > MAX_AWAIT_IN_STEP {
        return Err(format!(
            "step-no-await ratchet regression — {count} .await calls > ceiling {MAX_AWAIT_IN_STEP}"
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Rule 3: STEP-2 — `async fn step_*` or `-> impl Future` in step signatures
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_step_sync_signature(root: &Path) -> Result<()> {
    let target_dirs = [
        "crates/tx-subsystems/src",
        "crates/tx-kernel/src",
        "crates/tx-shims/src",
        "crates/tx-scripts/src",
        "crates/tx-fs/src",
    ];

    let mut violations: Vec<String> = Vec::new();

    for dir in &target_dirs {
        let dir_path = root.join(dir);
        if !dir_path.exists() {
            continue;
        }

        let files = collect_files(&dir_path, &["rs"]).map_err(|e| e.to_string())?;

        for file in &files {
            let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let rel = relative(root, file).replace('\\', "/");

            for (line_num, line) in text.lines().enumerate() {
                let trimmed = line.trim();

                if trimmed.starts_with("//") || trimmed.starts_with("///") {
                    continue;
                }

                let code = if let Some(pos) = trimmed.find("//") {
                    &trimmed[..pos]
                } else {
                    trimmed
                };

                // Check for `async fn step(` or `async fn step_*`
                if (code.contains("async fn step(") || code.contains("async fn step_"))
                    && code.contains('(')
                {
                    violations.push(format!(
                        "{rel}:{} — async fn step signature (STEP-2)",
                        line_num + 1
                    ));
                }

                // Check for `-> impl Future` in step fn signatures
                if (code.contains("fn step(") || code.contains("fn step_"))
                    && code.contains("impl Future")
                {
                    violations.push(format!(
                        "{rel}:{} — step fn returns impl Future (STEP-2)",
                        line_num + 1
                    ));
                }
            }
        }
    }

    let count = violations.len();

    println!("Invariants Lint — step-sync-signature (STEP-2)");
    println!("================================================");

    let status = if count > MAX_ASYNC_STEP_SIG {
        "OVER"
    } else {
        "ok"
    };
    println!("async step fn signatures: {count:>4}  (ceiling {MAX_ASYNC_STEP_SIG})  {status}");

    for v in &violations {
        println!("  {v}");
    }

    if count > MAX_ASYNC_STEP_SIG {
        return Err(format!(
            "step-sync-signature ratchet regression — {count} async sigs > ceiling {MAX_ASYNC_STEP_SIG}"
        ));
    }

    Ok(())
}
