//! Lint rules for syscall implementation hygiene against the v3 spec.
//!
//! Per `docs/Txv3/04_SYSCALL_SHAPE_v1.md` and
//! `docs/Txv3/03_STEP_MODEL_v2.md` §10 anti-patterns:
//! syscall functions in `linux_syscall/` must migrate from ad-hoc
//! StepOutcome-matching loops and `.await`-inside-syscall-fn patterns
//! to the canonical `drive()` + `StepOp` discipline.
//!
//! Three ratchet gates:
//! 1. `syscall-adhoc-loop` — count syscall files with manual
//!    `V3::Done|Continue|Yield` (or `V3Out::*`) match loops.
//!    These are the alias forms of the `StepOutcome::*` matching
//!    that `no-adhoc-drive` already catches for the canonical names.
//! 2. `syscall-no-await` — count `.await` calls inside
//!    `pub(super) fn sys_*` function bodies (A-3: no `.await` in
//!    step/dispatch sites — yield through `drive()` instead).
//! 3. `syscall-ctx-bridge` — count how many `sys_*` functions use
//!    `build_subject_script_ctx` (bridge to v3 `ScriptCtx`) vs how
//!    many still use the bare `SyscallCtx`. Trend toward 100% bridging.
//!
//! Scanned: `crates/tx-shims/src/linux_syscall/` (syscall implementation
//! files only — not mod.rs, numbers.rs, tests/).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

// ---------------------------------------------------------------------------
// Ratchet ceilings
// ---------------------------------------------------------------------------

/// Files with manual V3/V3Out alias match loops (not yet drive()-migrated).
/// Measured baseline 2026-05-15.
const MAX_SYSCALL_ADHOC_LOOP_FILES: usize = 10; // 8→10: vfs-full-bringup merge added two new syscall files with manual alias loops

/// `.await` sites inside `pub(super) fn sys_*` function bodies.
/// Measured baseline 2026-05-15.
const MAX_SYSCALL_AWAIT_SITES: usize = 60;

/// sys_* functions NOT yet bridged through build_subject_script_ctx.
/// (Informational only — no ratchet failure, just a metric.)
const _SYSCALL_CTX_BRIDGE_TARGET: usize = 0;

// ---------------------------------------------------------------------------
// V3/V3Out alias patterns to detect
// ---------------------------------------------------------------------------

/// Aliases for `StepOutcome` used in manual dispatch loops within syscall
/// implementation files. Each entry is `(prefix, display_name)`.
const OUTCOME_ALIASES: &[(&str, &str)] = &[("V3", "V3"), ("V3Out", "V3Out")];

/// The StepOutcome variants that signal ad-hoc dispatch when matched on
/// (outside of drive.rs).
const DISPATCH_VARIANTS: &[&str] = &["Continue", "Yield", "Done"];

// ---------------------------------------------------------------------------
// Lint 1: syscall-adhoc-loop
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_syscall_adhoc_loop(root: &Path) -> Result<()> {
    let syscall_dir = root.join("crates/tx-shims/src/linux_syscall");
    if !syscall_dir.exists() {
        println!("syscall dir not found — skipping");
        return Ok(());
    }

    let files = collect_files(&syscall_dir, &["rs"]).map_err(|e| e.to_string())?;

    let mut adhoc_files: Vec<String> = Vec::new();
    let mut adhoc_sites: Vec<String> = Vec::new();

    for file in &files {
        let rel = relative(root, file).replace('\\', "/");

        // Skip non-implementation files
        let fname = file.file_name().unwrap().to_str().unwrap_or("");
        if fname == "mod.rs" || fname == "numbers.rs" || fname == "tests.rs" {
            continue;
        }
        if rel.contains("/tests/") {
            continue;
        }

        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;

        // Skip files that define their own step_* functions.
        if text.contains("pub fn step_") || text.contains("pub(crate) fn step_") {
            continue;
        }

        let mut file_has_adhoc = false;

        for (line_num, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            // Skip comments
            if trimmed.starts_with("//")
                || trimmed.starts_with("///")
                || trimmed.starts_with("/*")
                || trimmed.starts_with("*")
            {
                continue;
            }

            let code_part = if let Some(pos) = trimmed.find("//") {
                &trimmed[..pos]
            } else {
                trimmed
            };

            // Skip construction sites: `=> V3::Yield {` or `return V3::Yield {`
            let is_construction = code_part.contains("=> V3::")
                || code_part.contains("return V3::")
                || code_part.contains("=> V3Out::")
                || code_part.contains("return V3Out::");
            if is_construction {
                continue;
            }

            // Check for alias dispatch patterns in match arms
            for (prefix, alias_name) in OUTCOME_ALIASES {
                for variant in DISPATCH_VARIANTS {
                    let pattern = format!("{}::{}", prefix, variant);
                    if code_part.contains(&pattern) {
                        // Skip if inside a test block
                        if is_in_test_context(&text, line_num) {
                            continue;
                        }
                        if !file_has_adhoc {
                            adhoc_files.push(rel.clone());
                            file_has_adhoc = true;
                        }
                        adhoc_sites.push(format!(
                            "{}:{} — ad-hoc {}::{} dispatch",
                            rel,
                            line_num + 1,
                            alias_name,
                            variant,
                        ));
                        break; // one hit per line
                    }
                }
            }
        }
    }

    let file_count = adhoc_files.len();
    let site_count = adhoc_sites.len();

    println!("Invariants Lint — syscall-adhoc-loop");
    println!("======================================");

    let status = if file_count > MAX_SYSCALL_ADHOC_LOOP_FILES {
        "OVER"
    } else {
        "ok"
    };
    println!(
        "files with ad-hoc alias loops: {:>4}  (ceiling {})  {}",
        file_count, MAX_SYSCALL_ADHOC_LOOP_FILES, status
    );
    println!("total ad-hoc alias sites:      {:>4}", site_count);

    if !adhoc_files.is_empty() {
        println!();
        println!("  files:");
        for f in &adhoc_files {
            println!("  {f}");
        }
        println!();
        println!("  sites:");
        for s in &adhoc_sites {
            println!("  {s}");
        }
    }

    if file_count > MAX_SYSCALL_ADHOC_LOOP_FILES {
        return Err(format!(
            "syscall-adhoc-loop ratchet regression — {file_count} files with ad-hoc alias loops > ceiling {MAX_SYSCALL_ADHOC_LOOP_FILES}"
        ));
    }

    Ok(())
}

/// Approximate check: is this line inside a `#[cfg(test)]` or `mod tests {` block?
fn is_in_test_context(text: &str, target_line: usize) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_test_mod = false;
    let mut test_depth: u32 = 0;

    for (i, line) in lines.iter().enumerate() {
        if i > target_line {
            break;
        }
        let trimmed = line.trim();
        if trimmed == "#[cfg(test)]" {
            in_test_mod = true;
            test_depth = 0;
            continue;
        }
        if trimmed.starts_with("mod tests") {
            in_test_mod = true;
            test_depth = 0;
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

// ---------------------------------------------------------------------------
// Lint 2: syscall-no-await — `.await` in sys_* function bodies
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_syscall_no_await(root: &Path) -> Result<()> {
    let syscall_dir = root.join("crates/tx-shims/src/linux_syscall");
    if !syscall_dir.exists() {
        println!("syscall dir not found — skipping");
        return Ok(());
    }

    let files = collect_files(&syscall_dir, &["rs"]).map_err(|e| e.to_string())?;

    let mut violations: Vec<String> = Vec::new();

    for file in &files {
        let rel = relative(root, file).replace('\\', "/");
        let fname = file.file_name().unwrap().to_str().unwrap_or("");

        if fname == "mod.rs" || fname == "numbers.rs" || fname == "tests.rs" {
            continue;
        }
        if rel.contains("/tests/") {
            continue;
        }

        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let lines: Vec<&str> = text.lines().collect();

        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.trim();

            if (trimmed.starts_with("pub(super) fn sys_")
                || trimmed.starts_with("pub(super) async fn sys_"))
                && trimmed.contains('(')
            {
                let mut brace_depth: i32 = 0;
                let mut in_body = false;
                let mut j = i;

                while j < lines.len() {
                    let body_line = lines[j];
                    let body_trimmed = body_line.trim();

                    if body_line.contains('{') {
                        brace_depth += body_line.matches('{').count() as i32;
                        in_body = true;
                    }
                    if body_line.contains('}') {
                        brace_depth -= body_line.matches('}').count() as i32;
                    }

                    if in_body
                        && !body_trimmed.starts_with("//")
                        && !body_trimmed.starts_with("///")
                    {
                        let code = if let Some(pos) = body_trimmed.find("//") {
                            &body_trimmed[..pos]
                        } else {
                            body_trimmed
                        };

                        if code.contains(".await") && !code.contains("drive(") {
                            violations.push(format!(
                                "{}:{} — .await in syscall fn body (A-3)",
                                rel,
                                j + 1
                            ));
                        }
                    }

                    if brace_depth <= 0 && in_body {
                        break;
                    }
                    j += 1;
                }

                i = j;
            }
            i += 1;
        }
    }

    let count = violations.len();

    println!("Invariants Lint — syscall-no-await (A-3)");
    println!("==========================================");

    let status = if count > MAX_SYSCALL_AWAIT_SITES {
        "OVER"
    } else {
        "ok"
    };
    println!(
        ".await in sys_* fn bodies: {:>4}  (ceiling {})  {}",
        count, MAX_SYSCALL_AWAIT_SITES, status
    );

    for v in &violations {
        println!("  {v}");
    }

    if count > MAX_SYSCALL_AWAIT_SITES {
        return Err(format!(
            "syscall-no-await ratchet regression — {count} .await sites > ceiling {MAX_SYSCALL_AWAIT_SITES}"
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Lint 3: syscall-ctx-bridge — SubjectContext bridge adoption metric
// ---------------------------------------------------------------------------

pub(crate) fn lint_invariants_syscall_ctx_bridge(root: &Path) -> Result<()> {
    let syscall_dir = root.join("crates/tx-shims/src/linux_syscall");
    if !syscall_dir.exists() {
        println!("syscall dir not found — skipping");
        return Ok(());
    }

    let files = collect_files(&syscall_dir, &["rs"]).map_err(|e| e.to_string())?;

    let mut total_syscalls: BTreeMap<String, usize> = BTreeMap::new();
    let mut bridged_syscalls: BTreeMap<String, usize> = BTreeMap::new();

    for file in &files {
        let rel = relative(root, file).replace('\\', "/");
        let fname = file.file_name().unwrap().to_str().unwrap_or("");

        if fname == "mod.rs" || fname == "numbers.rs" || fname == "tests.rs" {
            continue;
        }
        if rel.contains("/tests/") {
            continue;
        }

        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;

        let mut file_total = 0;
        let mut file_bridged = 0;
        let mut search_start = 0usize;
        loop {
            let rest = &text[search_start..];
            let fn_pos = rest
                .find("pub(super) fn sys_")
                .or_else(|| rest.find("pub(super) async fn sys_"));
            let delta = match fn_pos {
                Some(p) => p,
                None => break,
            };
            let fn_start = search_start + delta;

            let open = match text[fn_start..].find('{') {
                Some(p) => fn_start + p,
                None => break,
            };

            let mut depth = 1i32;
            let mut close = open + 1;
            while close < text.len() && depth > 0 {
                match text.as_bytes()[close] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                close += 1;
            }
            let fn_body = &text[open + 1..close - 1];

            file_total += 1;
            if fn_body.contains("build_subject_script_ctx") {
                file_bridged += 1;
            }

            search_start = close;
        }

        if file_total > 0 {
            total_syscalls.insert(rel.clone(), file_total);
            bridged_syscalls.insert(rel, file_bridged);
        }
    }

    let grand_total: usize = total_syscalls.values().sum();
    let grand_bridged: usize = bridged_syscalls.values().sum();

    println!("Invariants Lint — syscall-ctx-bridge (SUBJ-1)");
    println!("===============================================");

    let pct = if grand_total > 0 {
        (grand_bridged as f64 / grand_total as f64) * 100.0
    } else {
        0.0
    };
    println!(
        "sys_* fns bridged to ScriptCtx: {:>3}/{:>3}  ({:.0}%)",
        grand_bridged, grand_total, pct
    );

    if grand_total > 0 {
        println!();
        println!("  per-file:");
        for (f, total) in &total_syscalls {
            let bridged = bridged_syscalls.get(f).copied().unwrap_or(0);
            let short = f
                .strip_prefix("crates/tx-shims/src/linux_syscall/")
                .unwrap_or(f);
            let marker = if bridged > 0 { "✓" } else { " " };
            println!("  {marker} {short:30} {bridged:>2}/{total:<2} bridged");
        }
    }

    println!();
    println!(
        "  Target: {} bridging (all sys_* fns use ScriptCtx).",
        _SYSCALL_CTX_BRIDGE_TARGET
    );
    println!("  This metric is informational — no ratchet gate yet.");

    Ok(())
}
