//! Lint/report rule for Step interface migration surface.
//!
//! This is a survey rule, not a failing ratchet. It answers: how much
//! production code still exposes old-style `fn step_*` surfaces, and where do
//! `impl StepOp` wrappers already exist?

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::Result;
use crate::util::{collect_files, relative};

const TARGET_DIRS: &[&str] = &[
    "crates/tx-subsystems/src",
    "crates/tx-shims/src",
    "crates/tx-scripts/src",
    "crates/tx-kernel/src",
    "crates/tx-fs/src",
    "crates/tx-ext4/src",
    "crates/tx-ext4-format/src",
];

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StepInterfaceKind {
    LegacyStepFn,
    StepOpImpl,
}

#[derive(Default)]
struct FileStats {
    legacy_step_fns: usize,
    stepop_impls: usize,
}

pub(crate) fn lint_invariants_step_interface(root: &Path) -> Result<()> {
    let mut by_file = BTreeMap::<String, FileStats>::new();
    let mut by_bucket = BTreeMap::<String, FileStats>::new();
    let mut legacy_sites = Vec::new();
    let mut stepop_sites = Vec::new();

    for dir in TARGET_DIRS {
        let dir_path = root.join(dir);
        if !dir_path.exists() {
            continue;
        }

        let files = collect_files(&dir_path, &["rs"]).map_err(|err| err.to_string())?;
        for file in files {
            let rel = relative(root, &file).replace('\\', "/");
            if should_skip_file(&rel) {
                continue;
            }

            let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
            let test_lines = test_context_lines(&text);
            let mut pending_impl = None::<(usize, String)>;
            for (line_idx, line) in text.lines().enumerate() {
                if test_lines.contains(&line_idx) {
                    pending_impl = None;
                    continue;
                }
                let Some(code) = code_before_comment(line).map(str::trim) else {
                    continue;
                };
                if code.is_empty() {
                    continue;
                }

                if let Some((start_idx, header)) = pending_impl.as_mut() {
                    header.push(' ');
                    header.push_str(code);
                    if impl_header_complete(code) {
                        if is_stepop_impl_header(header) {
                            let site = format!(
                                "{rel}:{} - {}",
                                *start_idx + 1,
                                header.chars().take(120).collect::<String>()
                            );
                            let stats = by_file.entry(rel.clone()).or_default();
                            let bucket_stats =
                                by_bucket.entry(step_interface_bucket(&rel)).or_default();
                            stats.stepop_impls += 1;
                            bucket_stats.stepop_impls += 1;
                            stepop_sites.push(site);
                        }
                        pending_impl = None;
                    }
                    continue;
                }

                if code.starts_with("impl") {
                    if impl_header_complete(code) {
                        if is_stepop_impl_header(code) {
                            let stats = by_file.entry(rel.clone()).or_default();
                            let bucket_stats =
                                by_bucket.entry(step_interface_bucket(&rel)).or_default();
                            stats.stepop_impls += 1;
                            bucket_stats.stepop_impls += 1;
                            stepop_sites.push(format!(
                                "{rel}:{} - {}",
                                line_idx + 1,
                                code.chars().take(120).collect::<String>()
                            ));
                        }
                    } else {
                        pending_impl = Some((line_idx, code.to_string()));
                    }
                    continue;
                }

                if contains_fn_step(code) {
                    let stats = by_file.entry(rel.clone()).or_default();
                    let bucket_stats = by_bucket.entry(step_interface_bucket(&rel)).or_default();
                    stats.legacy_step_fns += 1;
                    bucket_stats.legacy_step_fns += 1;
                    legacy_sites.push(format!(
                        "{rel}:{} - {}",
                        line_idx + 1,
                        code.chars().take(120).collect::<String>()
                    ));
                }
            }
        }
    }

    let legacy_total: usize = by_file.values().map(|stats| stats.legacy_step_fns).sum();
    let stepop_total: usize = by_file.values().map(|stats| stats.stepop_impls).sum();
    let files_with_legacy = by_file
        .values()
        .filter(|stats| stats.legacy_step_fns > 0)
        .count();
    let files_without_stepop = by_file
        .values()
        .filter(|stats| stats.legacy_step_fns > 0 && stats.stepop_impls == 0)
        .count();

    println!("Invariants Lint - step-interface");
    println!("=================================");
    println!("legacy `fn step_*` surfaces:       {legacy_total:>4}");
    println!("`impl StepOp` wrappers:            {stepop_total:>4}");
    println!("files with legacy step surfaces:   {files_with_legacy:>4}");
    println!("legacy files with no StepOp impls: {files_without_stepop:>4}");

    if !by_bucket.is_empty() {
        println!();
        println!("  by bucket:");
        println!("  {:<42} {:>7} {:>7}", "bucket", "step_fn", "StepOp");
        for (bucket, stats) in &by_bucket {
            if stats.legacy_step_fns == 0 && stats.stepop_impls == 0 {
                continue;
            }
            println!(
                "  {bucket:<42} {:>7} {:>7}",
                stats.legacy_step_fns, stats.stepop_impls
            );
        }
    }

    let old_only_files: Vec<_> = by_file
        .iter()
        .filter(|(_, stats)| stats.legacy_step_fns > 0 && stats.stepop_impls == 0)
        .collect();
    if !old_only_files.is_empty() {
        println!();
        println!("  legacy-only files (first 40):");
        for (file, stats) in old_only_files.iter().take(40) {
            println!("  {file}: {} step fn(s)", stats.legacy_step_fns);
        }
        if old_only_files.len() > 40 {
            println!("  ... and {} more", old_only_files.len() - 40);
        }
    }

    if !legacy_sites.is_empty() {
        println!();
        println!("  legacy step sites (first 40):");
        for site in legacy_sites.iter().take(40) {
            println!("  {site}");
        }
        if legacy_sites.len() > 40 {
            println!("  ... and {} more", legacy_sites.len() - 40);
        }
    }

    if !stepop_sites.is_empty() {
        println!();
        println!("  StepOp impl sites (first 20):");
        for site in stepop_sites.iter().take(20) {
            println!("  {site}");
        }
        if stepop_sites.len() > 20 {
            println!("  ... and {} more", stepop_sites.len() - 20);
        }
    }

    Ok(())
}

#[cfg(test)]
fn classify_step_interface_line(line: &str) -> Option<StepInterfaceKind> {
    let code = code_before_comment(line)?.trim();
    if code.is_empty() {
        return None;
    }
    if is_stepop_impl_header(code) {
        return Some(StepInterfaceKind::StepOpImpl);
    }
    if contains_fn_step(code) {
        return Some(StepInterfaceKind::LegacyStepFn);
    }
    None
}

fn contains_fn_step(code: &str) -> bool {
    code.contains("fn step_") && !code.starts_with("macro_rules!")
}

fn impl_header_complete(code: &str) -> bool {
    code.contains('{') || code.ends_with(';')
}

fn is_stepop_impl_header(code: &str) -> bool {
    !code.contains("OneShotStepOp")
        && code.contains("StepOp")
        && (code.contains(" for ") || code.trim_start().starts_with("for "))
}

fn should_skip_file(rel: &str) -> bool {
    rel.contains("/tests/")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("_test.rs")
        || rel.ends_with("_tests.rs")
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

fn step_interface_bucket(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() >= 4 && parts[0] == "crates" {
        let crate_name = parts[1];
        if crate_name == "tx-subsystems" && parts.len() >= 5 {
            return format!("{crate_name}/{}", parts[3]);
        }
        if crate_name == "tx-shims" && parts.len() >= 5 {
            return format!("{crate_name}/{}", parts[3]);
        }
        return crate_name.to_string();
    }
    rel.split('/').next().unwrap_or(rel).to_string()
}

fn test_context_lines(text: &str) -> BTreeSet<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let mut skipped = BTreeSet::new();
    let mut in_test_mod = false;
    let mut test_depth = 0u32;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        if idx > 0 && lines[idx - 1].trim() == "#[test]" {
            skipped.insert(idx);
        }
        if trimmed == "#[cfg(test)]" || trimmed.starts_with("mod tests") {
            in_test_mod = true;
            test_depth = 0;
        }

        if in_test_mod {
            skipped.insert(idx);
            let opens = trimmed.matches('{').count() as u32;
            let closes = trimmed.matches('}').count() as u32;
            if opens > 0 && test_depth == 0 {
                test_depth = opens.saturating_sub(closes);
            } else {
                test_depth += opens;
                test_depth = test_depth.saturating_sub(closes);
            }
            if test_depth == 0 && idx > 0 && trimmed.contains('}') {
                in_test_mod = false;
            }
        }
    }

    skipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_step_function_lines() {
        assert_eq!(
            classify_step_interface_line("pub fn step_read() -> StepOutcome<usize> {"),
            Some(StepInterfaceKind::LegacyStepFn)
        );
    }

    #[test]
    fn classifies_stepop_impl_lines() {
        assert_eq!(
            classify_step_interface_line("impl<I: SubjectIdentity> StepOp<I> for ReadOp<'_> {"),
            Some(StepInterfaceKind::StepOpImpl)
        );
    }

    #[test]
    fn does_not_count_oneshot_stepop_as_stepop_wrapper() {
        assert_eq!(
            classify_step_interface_line("impl OneShotStepOp<ProcessIdentity> for ReadOp<'_> {}"),
            None
        );
    }

    #[test]
    fn classifies_qualified_multiline_stepop_impl_lines() {
        assert_eq!(
            classify_step_interface_line(
                "crate::thread_runtime::adapter::step_engine::StepOp<I> for ThreadExitOp"
            ),
            Some(StepInterfaceKind::StepOpImpl)
        );
    }

    #[test]
    fn classifies_accumulated_multiline_stepop_impl_header() {
        let mut header = String::from("impl<I: SubjectIdentity>");
        header.push_str(" StepOp<I>");
        header.push_str(" for ReadOp<'_> {");
        assert!(is_stepop_impl_header(&header));
    }

    #[test]
    fn does_not_treat_stepop_import_as_impl_header() {
        assert_eq!(
            classify_step_interface_line("use tx_substrate::step::{ScriptCtx, StepOp};"),
            None
        );
    }

    #[test]
    fn skips_comments() {
        assert_eq!(
            classify_step_interface_line("// pub fn step_old() {}"),
            None
        );
    }

    #[test]
    fn buckets_subsystems_by_first_module() {
        assert_eq!(
            step_interface_bucket("crates/tx-subsystems/src/vfs/execution.rs"),
            "tx-subsystems/vfs"
        );
    }
}
