//! Lint rule: notification boundary convergence ratchet.
//!
//! Raw notification primitives should converge into subsystem adapters and
//! subsystem-local `notification.rs` semantic wrappers.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

/// Ratchet ceiling: raw notification primitive uses outside convergence homes.
///
/// Baseline lowered to zero on 2026-05-24 after migrating subsystem
/// notification wrappers.
const MAX_NOTIFICATION_BOUNDARY_VIOLATIONS: usize = 0;

const RAW_NOTIFICATION_PATTERNS: &[&str] = &[
    "wait_source::register_wait_channel",
    "wait_source::release_wait_channel",
    "wait_source::wait_on_token",
    "tx_substrate::wake::notify",
    "tx_substrate::wake::new_source",
    "tx_substrate::wake::register_source",
    "tx_substrate::wake::unregister_source",
    "tx_reactor::wait::fire_legacy",
    "Mask::from_bits",
    "YieldShape::OnWaitSource",
    "StepOutcome::yield_on_wait_source",
];

pub(crate) fn lint_invariants_notification_boundary(root: &Path) -> Result<()> {
    let subsystems = root.join("crates/tx-subsystems/src");
    if !subsystems.exists() {
        return Ok(());
    }

    let mut sites = Vec::new();
    let mut by_pattern: BTreeMap<&'static str, usize> = BTreeMap::new();

    let files = collect_files(&subsystems, &["rs"]).map_err(|err| err.to_string())?;
    for file in files {
        let rel = relative(root, &file).replace('\\', "/");
        if should_skip_file(&rel) {
            continue;
        }

        let text = fs::read_to_string(&file).map_err(|err| format!("{}: {err}", rel))?;
        if is_convergence_home(&rel) {
            continue;
        }

        for (line_idx, line) in text.lines().enumerate() {
            let Some(code) = code_before_comment(line) else {
                continue;
            };
            if is_in_test_context(&text, line_idx)
                || is_in_notification_adapter_scope(&text, line_idx)
            {
                continue;
            }

            for pattern in RAW_NOTIFICATION_PATTERNS {
                if code.contains(pattern) {
                    *by_pattern.entry(pattern).or_insert(0) += 1;
                    sites.push(format!(
                        "{rel}:{} - raw notification primitive `{pattern}`: {}",
                        line_idx + 1,
                        code.trim().chars().take(110).collect::<String>()
                    ));
                }
            }
        }
    }

    let count = sites.len();
    let over = count > MAX_NOTIFICATION_BOUNDARY_VIOLATIONS;

    println!("Invariants Lint - notification-boundary");
    println!("========================================");
    println!(
        "raw notification sites outside convergence homes: {:>4}  (ceiling {})  {}",
        count,
        MAX_NOTIFICATION_BOUNDARY_VIOLATIONS,
        if over { "OVER" } else { "ok" }
    );

    if !by_pattern.is_empty() {
        println!();
        println!("  by pattern:");
        for pattern in RAW_NOTIFICATION_PATTERNS {
            if let Some(n) = by_pattern.get(pattern) {
                println!("  {pattern:<52} {n:>4}");
            }
        }
    }

    if !sites.is_empty() {
        println!();
        println!("  sites (first 60):");
        for site in sites.iter().take(60) {
            println!("  {site}");
        }
        if sites.len() > 60 {
            println!("  ... and {} more", sites.len() - 60);
        }
    }

    if over {
        return Err(format!(
            "notification-boundary ratchet regression - {count} raw notification sites outside adapter.rs / notification.rs / #[notification_adapter] scopes > ceiling {MAX_NOTIFICATION_BOUNDARY_VIOLATIONS}. Move raw primitives behind adapter.rs and subsystem semantic notification wrappers before lowering the ratchet."
        ));
    }

    Ok(())
}

fn should_skip_file(rel: &str) -> bool {
    rel == "crates/tx-subsystems/src/wait_source.rs"
        || rel.contains("/tests/")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("_test.rs")
}

fn is_convergence_home(rel: &str) -> bool {
    rel.ends_with("/adapter.rs") || rel.ends_with("/notification.rs")
}

fn code_before_comment(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*")
    {
        return None;
    }
    Some(line.split_once("//").map(|(code, _)| code).unwrap_or(line))
}

fn is_in_test_context(text: &str, target_line: usize) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_test_mod = false;
    let mut test_depth = 0u32;

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

fn is_in_notification_adapter_scope(text: &str, target_line: usize) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let mut pending_attr = false;
    let mut in_scope = false;
    let mut scope_depth = 0u32;

    for (i, line) in lines.iter().enumerate() {
        if i > target_line {
            break;
        }

        let trimmed = line.trim();
        if !in_scope && trimmed.starts_with("#[notification_adapter(") {
            pending_attr = true;
            continue;
        }

        if pending_attr && !in_scope && trimmed.starts_with("mod ") && trimmed.contains('{') {
            in_scope = true;
            pending_attr = false;
        }

        if in_scope {
            let opens = trimmed.matches('{').count() as u32;
            let closes = trimmed.matches('}').count() as u32;
            scope_depth = scope_depth.saturating_add(opens).saturating_sub(closes);
            if i == target_line {
                return true;
            }
            if scope_depth == 0 {
                in_scope = false;
            }
        }
    }

    false
}
